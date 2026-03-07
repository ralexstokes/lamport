mod support;

use lamport::{Envelope, mailbox::Mailbox};
use support::expect_downcast;

#[derive(Clone, Copy)]
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u32(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        (self.0 >> 32) as u32
    }

    fn next_usize(&mut self, upper: usize) -> usize {
        if upper == 0 {
            0
        } else {
            (self.next_u32() as usize) % upper
        }
    }
}

fn envelope_u32(envelope: Envelope) -> u32 {
    match envelope {
        Envelope::User(payload) => expect_downcast::<u32>(payload, "mailbox property payload"),
        other => panic!("unexpected envelope: {other:?}"),
    }
}

fn is_value(envelope: &Envelope, expected: u32) -> bool {
    matches!(envelope, Envelope::User(payload) if payload.downcast_ref::<u32>() == Some(&expected))
}

fn push_next(mailbox: &mut Mailbox, model: &mut Vec<u32>, next_value: &mut u32) {
    mailbox.push(Envelope::user(*next_value));
    model.push(*next_value);
    *next_value += 1;
}

#[test]
fn generated_sequences_match_fifo_and_selective_receive_model() {
    for seed in 0..32_u64 {
        let mut rng = Lcg::new(seed + 1);
        let mut mailbox = Mailbox::new();
        let mut model = Vec::new();
        let mut next_value = 0_u32;

        for _ in 0..400 {
            match rng.next_usize(4) {
                0 | 1 if model.is_empty() => {
                    push_next(&mut mailbox, &mut model, &mut next_value);
                }
                0 => {
                    push_next(&mut mailbox, &mut model, &mut next_value);
                }
                1 => {
                    let expected = if model.is_empty() {
                        None
                    } else {
                        Some(model.remove(0))
                    };
                    let actual = mailbox.pop_front().map(envelope_u32);
                    assert_eq!(actual, expected, "seed={seed}");
                }
                2 if model.is_empty() => {
                    push_next(&mut mailbox, &mut model, &mut next_value);
                }
                2 => {
                    let index = rng.next_usize(model.len());
                    let target = model[index];
                    let actual = mailbox
                        .selective_receive(|envelope| is_value(envelope, target))
                        .map(envelope_u32);
                    assert_eq!(actual, Some(target), "seed={seed}");
                    assert_eq!(model.remove(index), target, "seed={seed}");
                }
                _ => {
                    push_next(&mut mailbox, &mut model, &mut next_value);
                }
            }
        }

        for expected in model {
            assert_eq!(mailbox.pop_front().map(envelope_u32), Some(expected));
        }
        assert!(mailbox.pop_front().is_none());
    }
}

#[test]
fn generated_watermarks_only_match_future_messages() {
    for seed in 0..32_u64 {
        let mut rng = Lcg::new(seed ^ 0x9e37_79b9_7f4a_7c15);

        for _ in 0..80 {
            let mut mailbox = Mailbox::new();
            let mut model = Vec::new();
            let mut next_value = 0_u32;
            let mut next_sequence = 0_u64;

            let prefix_len = 1 + rng.next_usize(8);
            for _ in 0..prefix_len {
                mailbox.push(Envelope::user(next_value));
                model.push((next_sequence, next_value));
                next_sequence += 1;
                next_value += 1;
            }

            let watermark = mailbox.watermark();
            let watermark_sequence = next_sequence;

            let suffix_len = 1 + rng.next_usize(8);
            for _ in 0..suffix_len {
                mailbox.push(Envelope::user(next_value));
                model.push((next_sequence, next_value));
                next_sequence += 1;
                next_value += 1;
            }

            let target = if rng.next_usize(2) == 0 {
                let future: Vec<_> = model
                    .iter()
                    .filter_map(|(sequence, value)| {
                        (*sequence >= watermark_sequence).then_some(*value)
                    })
                    .collect();
                future[rng.next_usize(future.len())]
            } else {
                let older: Vec<_> = model
                    .iter()
                    .filter_map(|(sequence, value)| {
                        (*sequence < watermark_sequence).then_some(*value)
                    })
                    .collect();

                if older.is_empty() {
                    let future: Vec<_> = model
                        .iter()
                        .filter_map(|(sequence, value)| {
                            (*sequence >= watermark_sequence).then_some(*value)
                        })
                        .collect();
                    future[rng.next_usize(future.len())]
                } else {
                    older[rng.next_usize(older.len())]
                }
            };

            let expected = model
                .iter()
                .position(|(sequence, value)| *sequence >= watermark_sequence && *value == target)
                .map(|index| model.remove(index).1);

            let actual = mailbox
                .selective_receive_after(watermark, |envelope| is_value(envelope, target))
                .map(envelope_u32);

            assert_eq!(actual, expected, "seed={seed} target={target}");

            let remaining: Vec<_> = std::iter::from_fn(|| mailbox.pop_front())
                .map(envelope_u32)
                .collect();
            let expected_remaining: Vec<_> = model.into_iter().map(|(_, value)| value).collect();
            assert_eq!(remaining, expected_remaining, "seed={seed}");
        }
    }
}
