use std::collections::VecDeque;

use crate::envelope::{Envelope, EnvelopeKind};

#[derive(Debug)]
struct QueuedEnvelope {
    sequence: u64,
    envelope: Envelope,
}

/// A stable mailbox watermark used to skip older entries during selective receive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MailboxWatermark(u64);

impl MailboxWatermark {
    /// Creates a watermark from the next mailbox sequence number.
    pub const fn new(sequence: u64) -> Self {
        Self(sequence)
    }

    /// Returns the next sequence number represented by the watermark.
    pub const fn sequence(self) -> u64 {
        self.0
    }
}

/// FIFO mailbox storage with `O(N)` selective receive.
#[derive(Debug)]
pub struct Mailbox {
    next_sequence: u64,
    queue: VecDeque<QueuedEnvelope>,
    capacity: usize,
    runtime_reserve: usize,
}

impl Default for Mailbox {
    fn default() -> Self {
        Self::new()
    }
}

/// Capacity failure while enqueuing into a bounded mailbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MailboxFull {
    /// Total mailbox capacity.
    pub capacity: usize,
}

impl Mailbox {
    /// Creates an empty mailbox.
    pub fn new() -> Self {
        Self {
            next_sequence: 0,
            queue: VecDeque::new(),
            capacity: usize::MAX,
            runtime_reserve: 0,
        }
    }

    /// Creates a bounded mailbox.
    pub fn with_capacity(capacity: usize) -> Self {
        Self::with_limits(capacity, 0)
    }

    /// Creates a bounded mailbox with reserve slots for runtime-originated events.
    pub fn with_limits(capacity: usize, runtime_reserve: usize) -> Self {
        let capacity = capacity.max(1);
        let runtime_reserve = runtime_reserve.min(capacity.saturating_sub(1));

        Self {
            next_sequence: 0,
            queue: VecDeque::new(),
            capacity,
            runtime_reserve,
        }
    }

    /// Returns the number of queued envelopes.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Returns `true` when the mailbox has no queued envelopes.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Returns the mailbox capacity when bounded.
    pub fn capacity(&self) -> Option<usize> {
        (self.capacity != usize::MAX).then_some(self.capacity)
    }

    /// Returns the reserved slots kept available for runtime-originated events.
    pub fn runtime_reserve(&self) -> usize {
        self.runtime_reserve
    }

    /// Returns a watermark representing messages that will arrive in the future.
    pub fn watermark(&self) -> MailboxWatermark {
        MailboxWatermark::new(self.next_sequence)
    }

    /// Appends an envelope to the mailbox.
    pub fn push(&mut self, envelope: Envelope) {
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        self.queue.push_back(QueuedEnvelope { sequence, envelope });
    }

    /// Appends an envelope, returning a backpressure error when bounded capacity is exhausted.
    pub fn try_push(&mut self, envelope: Envelope) -> Result<(), MailboxFull> {
        if self.is_full_for(envelope.kind()) {
            return Err(MailboxFull {
                capacity: self.capacity,
            });
        }

        self.push(envelope);
        Ok(())
    }

    /// Pops the oldest queued envelope.
    pub fn pop_front(&mut self) -> Option<Envelope> {
        self.queue.pop_front().map(|entry| entry.envelope)
    }

    /// Removes the first queued envelope that matches the predicate.
    pub fn selective_receive<F>(&mut self, mut predicate: F) -> Option<Envelope>
    where
        F: FnMut(&Envelope) -> bool,
    {
        self.remove_first_matching(|entry| predicate(&entry.envelope))
    }

    /// Removes the first matching envelope whose sequence is at or after the watermark.
    pub fn selective_receive_after<F>(
        &mut self,
        watermark: MailboxWatermark,
        mut predicate: F,
    ) -> Option<Envelope>
    where
        F: FnMut(&Envelope) -> bool,
    {
        self.remove_first_matching(|entry| {
            entry.sequence >= watermark.sequence() && predicate(&entry.envelope)
        })
    }

    fn remove_first_matching<F>(&mut self, mut predicate: F) -> Option<Envelope>
    where
        F: FnMut(&QueuedEnvelope) -> bool,
    {
        for index in 0..self.queue.len() {
            if predicate(self.queue.get(index).expect("index within mailbox bounds")) {
                return self.queue.remove(index).map(|entry| entry.envelope);
            }
        }

        None
    }

    fn is_full_for(&self, kind: EnvelopeKind) -> bool {
        if self.capacity == usize::MAX {
            return false;
        }

        let len = self.queue.len();
        if kind.uses_runtime_reserve() {
            len >= self.capacity
        } else {
            len >= self.capacity.saturating_sub(self.runtime_reserve)
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::envelope::Envelope;

    use super::{Mailbox, MailboxFull};

    fn into_u32(envelope: Envelope) -> u32 {
        match envelope {
            Envelope::User(payload) => payload.downcast::<u32>().ok().unwrap(),
            other => panic!("unexpected envelope: {other:?}"),
        }
    }

    #[test]
    fn fifo_pop_preserves_send_order() {
        let mut mailbox = Mailbox::new();
        mailbox.push(Envelope::user(1_u32));
        mailbox.push(Envelope::user(2_u32));
        mailbox.push(Envelope::user(3_u32));

        assert_eq!(into_u32(mailbox.pop_front().unwrap()), 1);
        assert_eq!(into_u32(mailbox.pop_front().unwrap()), 2);
        assert_eq!(into_u32(mailbox.pop_front().unwrap()), 3);
        assert!(mailbox.pop_front().is_none());
    }

    #[test]
    fn selective_receive_preserves_unmatched_relative_order() {
        let mut mailbox = Mailbox::new();
        mailbox.push(Envelope::user(1_u32));
        mailbox.push(Envelope::user(2_u32));
        mailbox.push(Envelope::user(3_u32));

        let matched = mailbox.selective_receive(|envelope| {
            matches!(envelope, Envelope::User(payload) if payload.downcast_ref::<u32>() == Some(&2))
        });

        assert_eq!(into_u32(matched.unwrap()), 2);
        assert_eq!(into_u32(mailbox.pop_front().unwrap()), 1);
        assert_eq!(into_u32(mailbox.pop_front().unwrap()), 3);
    }

    #[test]
    fn selective_receive_after_skips_older_messages() {
        let mut mailbox = Mailbox::new();
        mailbox.push(Envelope::user(10_u32));
        let watermark = mailbox.watermark();
        mailbox.push(Envelope::user(20_u32));
        mailbox.push(Envelope::user(30_u32));

        let matched = mailbox.selective_receive_after(watermark, |envelope| {
            matches!(envelope, Envelope::User(payload) if payload.downcast_ref::<u32>() == Some(&20))
        });

        assert_eq!(into_u32(matched.unwrap()), 20);
        assert_eq!(into_u32(mailbox.pop_front().unwrap()), 10);
        assert_eq!(into_u32(mailbox.pop_front().unwrap()), 30);
    }

    #[test]
    fn bounded_mailbox_applies_backpressure() {
        let mut mailbox = Mailbox::with_capacity(1);

        mailbox.try_push(Envelope::user(1_u32)).unwrap();
        assert_eq!(
            mailbox.try_push(Envelope::user(2_u32)),
            Err(MailboxFull { capacity: 1 })
        );
    }

    #[test]
    fn runtime_reserve_keeps_one_slot_for_runtime_messages() {
        let mut mailbox = Mailbox::with_limits(2, 1);

        mailbox.try_push(Envelope::user(1_u32)).unwrap();
        assert_eq!(
            mailbox.try_push(Envelope::user(2_u32)),
            Err(MailboxFull { capacity: 2 })
        );
        mailbox
            .try_push(Envelope::call_timeout(crate::Ref::new(1)))
            .unwrap();
    }
}
