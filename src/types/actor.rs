use std::{
    fmt,
    sync::atomic::{AtomicU64, Ordering},
};

/// A stable local actor handle with a generation counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ActorId {
    /// The runtime-local slot for the actor.
    pub local_id: u64,
    /// The actor generation for stale-id detection after restarts.
    pub generation: u64,
}

impl ActorId {
    /// Creates a new actor id.
    pub const fn new(local_id: u64, generation: u64) -> Self {
        Self {
            local_id,
            generation,
        }
    }

    /// Returns the same local slot with the generation incremented.
    pub const fn next_generation(self) -> Self {
        Self {
            local_id: self.local_id,
            generation: self.generation + 1,
        }
    }
}

impl fmt::Display for ActorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.local_id, self.generation)
    }
}

/// A routable process address.
///
/// This is intentionally distinct from [`ActorId`]: today every address is
/// local, but the enum shape avoids hard-coding that assumption into the
/// routing-facing API surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ProcessAddr {
    /// A process running inside the current runtime instance.
    Local(ActorId),
}

impl ProcessAddr {
    /// Creates a local process address from an actor id.
    pub const fn local(actor: ActorId) -> Self {
        Self::Local(actor)
    }

    /// Returns the local actor id when the address points at a local process.
    pub const fn as_local(self) -> Option<ActorId> {
        match self {
            Self::Local(actor) => Some(actor),
        }
    }
}

impl From<ActorId> for ProcessAddr {
    fn from(value: ActorId) -> Self {
        Self::Local(value)
    }
}

impl PartialEq<ActorId> for ProcessAddr {
    fn eq(&self, other: &ActorId) -> bool {
        matches!(self, Self::Local(actor) if actor == other)
    }
}

impl PartialEq<ProcessAddr> for ActorId {
    fn eq(&self, other: &ProcessAddr) -> bool {
        other == self
    }
}

impl fmt::Display for ProcessAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local(actor) => actor.fmt(f),
        }
    }
}

/// A runtime-unique reference for monitors, calls, and timers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Ref(u64);

impl Ref {
    /// Creates a reference from a raw integer.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the raw integer value.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Allocates a fresh runtime reference.
    pub fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);

        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// A timer handle used to correlate fired and cancelled timers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TimerToken(Ref);

impl TimerToken {
    /// Creates a timer token from a reference.
    pub const fn from_ref(reference: Ref) -> Self {
        Self(reference)
    }

    /// Returns the underlying reference.
    pub const fn as_ref(self) -> Ref {
        self.0
    }

    /// Allocates a fresh timer token.
    pub fn next() -> Self {
        Self(Ref::next())
    }
}
