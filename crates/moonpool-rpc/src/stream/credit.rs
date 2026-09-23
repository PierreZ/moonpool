//! Checked stream credit: the producer's reservations and acknowledgement
//! validation, and the consumer's intake. Pure state, no I/O, no locks.
//!
//! Units: every counter is in **accounted bytes**, the whole frame of an
//! item ([`stream_item_frame_len`](crate::protocol::stream_item_frame_len)),
//! so both sides compute the same number from the same body without
//! trusting each other. Counters are cumulative `u64`s updated with checked
//! arithmetic: an overflow is a refusal, never a wrap.
//!
//! This is `FoundationDB`'s `AcknowledgementReceiver` (`bytesSent`,
//! `bytesAcknowledged`, `bytesLimit`, `sequence`) and the client half of
//! `NetNotifiedQueueWithAcknowledgements`, with two deliberate
//! differences: an item is sent only if it fits the window whole (FDB lets
//! one item overshoot once anything is free, `onReady`), so an item larger
//! than the window is refused instead of waiting; and an acknowledgement
//! must land on an item boundary the producer actually sent, so a forged,
//! regressing or excess one is detected instead of asserted.

use std::collections::VecDeque;

/// Why an item cannot be sent now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// Larger than the whole window: it can never be sent.
    TooLarge { size: u64, limit: u64 },
    /// Not enough credit right now; wait for acknowledgements.
    Wait,
    /// A cumulative counter would overflow.
    Overflow,
}

/// A valid acknowledgement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AckOutcome {
    /// Credit was returned.
    Advanced,
    /// The same cumulative value again: harmless, ignored.
    Duplicate,
}

/// An acknowledgement the producer refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AckViolation {
    /// Below an earlier acknowledgement.
    Regressing { acked: u64, got: u64 },
    /// More than was ever sent.
    Excess { sent: u64, got: u64 },
    /// Not on the boundary of an item that was sent (forged).
    Misaligned { got: u64 },
}

impl std::fmt::Display for AckViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Regressing { acked, got } => {
                write!(f, "acknowledgement {got} regresses below {acked}")
            }
            Self::Excess { sent, got } => {
                write!(f, "acknowledgement {got} exceeds the {sent} bytes sent")
            }
            Self::Misaligned { got } => {
                write!(f, "acknowledgement {got} is not on an item boundary")
            }
        }
    }
}

/// The producer's credit.
#[derive(Debug)]
pub(crate) struct Credit {
    window: u64,
    sent: u64,
    acked: u64,
    /// Cumulative `sent` after each item not yet acknowledged, oldest first.
    /// At most `window / smallest item` entries.
    boundaries: VecDeque<u64>,
    next_sequence: u64,
}

impl Credit {
    /// Credit for a stream whose consumer announced `window` bytes.
    pub(crate) fn new(window: u64) -> Self {
        Self::starting_at(window, 0, 0)
    }

    /// Credit whose counters start at `start` bytes and `sequence` items
    /// (tests drive counters to their limits this way).
    pub(crate) fn starting_at(window: u64, start: u64, sequence: u64) -> Self {
        Self {
            window,
            sent: start,
            acked: start,
            boundaries: VecDeque::new(),
            next_sequence: sequence,
        }
    }

    pub(crate) fn window(&self) -> u64 {
        self.window
    }

    /// Lower the window to `limit` (never raise it above what the consumer
    /// announced).
    pub(crate) fn limit_window(&mut self, limit: u64) {
        self.window = self.window.min(limit);
    }

    /// Bytes sent and not yet acknowledged.
    pub(crate) fn in_flight(&self) -> u64 {
        self.sent - self.acked
    }

    /// Items sent so far (the next item's sequence).
    pub(crate) fn items(&self) -> u64 {
        self.next_sequence
    }

    /// Whether any credit is free (`FoundationDB`'s `onReady` condition).
    pub(crate) fn has_room(&self) -> bool {
        self.in_flight() < self.window
    }

    /// Reserve `size` bytes for the next item, atomically with its
    /// sequence number, or say why not.
    pub(crate) fn reserve(&mut self, size: u64) -> Result<u64, Refusal> {
        if size > self.window {
            return Err(Refusal::TooLarge {
                size,
                limit: self.window,
            });
        }
        let sent = self.sent.checked_add(size).ok_or(Refusal::Overflow)?;
        let next = self.next_sequence.checked_add(1).ok_or(Refusal::Overflow)?;
        // `in_flight <= window` always holds, so this cannot wrap either way.
        if sent - self.acked > self.window {
            return Err(Refusal::Wait);
        }
        let sequence = self.next_sequence;
        self.sent = sent;
        self.next_sequence = next;
        self.boundaries.push_back(sent);
        Ok(sequence)
    }

    /// Apply a cumulative acknowledgement of `consumed` bytes.
    pub(crate) fn ack(&mut self, consumed: u64) -> Result<AckOutcome, AckViolation> {
        if consumed == self.acked {
            return Ok(AckOutcome::Duplicate);
        }
        if consumed < self.acked {
            return Err(AckViolation::Regressing {
                acked: self.acked,
                got: consumed,
            });
        }
        if consumed > self.sent {
            return Err(AckViolation::Excess {
                sent: self.sent,
                got: consumed,
            });
        }
        match self.boundaries.binary_search(&consumed) {
            Ok(index) => {
                self.boundaries.drain(..=index);
                self.acked = consumed;
                Ok(AckOutcome::Advanced)
            }
            Err(_) => Err(AckViolation::Misaligned { got: consumed }),
        }
    }
}

/// Why a consumer refuses what arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IntakeViolation {
    /// An item skipped ahead of the next expected sequence.
    Gap { expected: u64, got: u64 },
    /// An item repeated or went back in sequence.
    Reordered { expected: u64, got: u64 },
    /// A cumulative counter would overflow.
    Overflow,
    /// More unconsumed bytes than the announced window.
    WindowExceeded { buffered: u64, window: u64 },
    /// The end announced a different item count than was received.
    CountMismatch { announced: u64, received: u64 },
}

impl std::fmt::Display for IntakeViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Gap { expected, got } => {
                write!(f, "item {got} arrived while {expected} was expected (gap)")
            }
            Self::Reordered { expected, got } => write!(
                f,
                "item {got} arrived while {expected} was expected (repeated or reordered)"
            ),
            Self::Overflow => f.write_str("stream byte counter overflow"),
            Self::WindowExceeded { buffered, window } => write!(
                f,
                "{buffered} unconsumed bytes exceed the {window}-byte window"
            ),
            Self::CountMismatch {
                announced,
                received,
            } => write!(
                f,
                "the end announced {announced} items but {received} arrived"
            ),
        }
    }
}

/// The consumer's side of the accounting.
#[derive(Debug)]
pub(crate) struct Intake {
    window: u64,
    next_sequence: u64,
    received: u64,
    consumed: u64,
}

impl Intake {
    pub(crate) fn new(window: u64) -> Self {
        Self::starting_at(window, 0, 0)
    }

    /// An intake whose counters start at `start` bytes and `sequence`
    /// items (tests only).
    pub(crate) fn starting_at(window: u64, start: u64, sequence: u64) -> Self {
        Self {
            window,
            next_sequence: sequence,
            received: start,
            consumed: start,
        }
    }

    /// Received and not yet consumed.
    pub(crate) fn buffered(&self) -> u64 {
        self.received - self.consumed
    }

    /// Items received so far.
    pub(crate) fn items(&self) -> u64 {
        self.next_sequence
    }

    /// Accept item `sequence` of `size` accounted bytes.
    pub(crate) fn accept(&mut self, sequence: u64, size: u64) -> Result<(), IntakeViolation> {
        let expected = self.next_sequence;
        if sequence > expected {
            return Err(IntakeViolation::Gap {
                expected,
                got: sequence,
            });
        }
        if sequence < expected {
            return Err(IntakeViolation::Reordered {
                expected,
                got: sequence,
            });
        }
        let received = self
            .received
            .checked_add(size)
            .ok_or(IntakeViolation::Overflow)?;
        let next = expected.checked_add(1).ok_or(IntakeViolation::Overflow)?;
        let buffered = received - self.consumed;
        if buffered > self.window {
            return Err(IntakeViolation::WindowExceeded {
                buffered,
                window: self.window,
            });
        }
        self.received = received;
        self.next_sequence = next;
        Ok(())
    }

    /// The application took an item of `size` bytes: the new cumulative
    /// consumption to acknowledge. Never more than was received.
    pub(crate) fn consume(&mut self, size: u64) -> u64 {
        self.consumed = self.consumed.saturating_add(size).min(self.received);
        self.consumed
    }

    /// Check an end announcing `items` sent items.
    pub(crate) fn end(&self, items: u64) -> Result<(), IntakeViolation> {
        if items == self.next_sequence {
            Ok(())
        } else {
            Err(IntakeViolation::CountMismatch {
                announced: items,
                received: self.next_sequence,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AckOutcome, AckViolation, Credit, Intake, IntakeViolation, Refusal};
    use crate::protocol::stream_item_frame_len;

    /// Drive a producer and a consumer through the same items, the
    /// consumer acknowledging as its application takes them.
    #[test]
    fn first_acknowledgement_returns_exactly_the_first_item() {
        let item = stream_item_frame_len(10);
        let mut credit = Credit::new(3 * item);
        let mut intake = Intake::new(3 * item);
        // No item sent yet: nothing may be acknowledged but zero.
        assert_eq!(credit.ack(0), Ok(AckOutcome::Duplicate));
        assert_eq!(
            credit.ack(item),
            Err(AckViolation::Excess { sent: 0, got: item })
        );
        assert_eq!(credit.reserve(item), Ok(0));
        intake.accept(0, item).expect("first item");
        assert_eq!(credit.in_flight(), item);
        let ack = intake.consume(item);
        assert_eq!(ack, item);
        assert_eq!(credit.ack(ack), Ok(AckOutcome::Advanced));
        assert_eq!(credit.in_flight(), 0);
        assert_eq!(intake.buffered(), 0);
    }

    #[test]
    fn acknowledgements_are_cumulative_and_land_on_item_boundaries() {
        let mut credit = Credit::new(1000);
        for size in [100, 50, 200] {
            credit.reserve(size).expect("fits");
        }
        // Acknowledging two items at once returns both.
        assert_eq!(credit.ack(150), Ok(AckOutcome::Advanced));
        assert_eq!(credit.in_flight(), 200);
        // A duplicate is harmless; a regression, an excess or a value that
        // is not an item boundary is refused and changes nothing.
        assert_eq!(credit.ack(150), Ok(AckOutcome::Duplicate));
        assert_eq!(
            credit.ack(100),
            Err(AckViolation::Regressing {
                acked: 150,
                got: 100
            })
        );
        assert_eq!(
            credit.ack(351),
            Err(AckViolation::Excess {
                sent: 350,
                got: 351
            })
        );
        assert_eq!(credit.ack(300), Err(AckViolation::Misaligned { got: 300 }));
        assert_eq!(credit.in_flight(), 200);
        assert_eq!(credit.ack(350), Ok(AckOutcome::Advanced));
        assert_eq!(credit.in_flight(), 0);
    }

    #[test]
    fn credit_is_reserved_whole_and_exhausted_credit_waits() {
        let mut credit = Credit::new(300);
        assert_eq!(credit.reserve(200), Ok(0));
        assert!(credit.has_room());
        // Two concurrent senders: the second one cannot oversubscribe.
        assert_eq!(credit.reserve(100), Ok(1));
        assert_eq!(credit.reserve(1), Err(Refusal::Wait));
        assert!(!credit.has_room());
        assert_eq!(credit.items(), 2);
        assert_eq!(credit.in_flight(), 300);
        assert_eq!(credit.ack(200), Ok(AckOutcome::Advanced));
        assert_eq!(credit.reserve(201), Err(Refusal::Wait));
        assert_eq!(credit.reserve(200), Ok(2));
        assert_eq!(credit.in_flight(), 300);
    }

    #[test]
    fn an_item_the_size_of_the_window_fits_and_a_larger_one_never_waits() {
        let mut credit = Credit::new(stream_item_frame_len(64));
        let window = credit.window();
        assert_eq!(
            credit.reserve(window + 1),
            Err(Refusal::TooLarge {
                size: window + 1,
                limit: window
            })
        );
        assert_eq!(credit.reserve(window), Ok(0));
        // Refused as too large even while credit is exhausted: never a wait.
        assert_eq!(
            credit.reserve(window + 1),
            Err(Refusal::TooLarge {
                size: window + 1,
                limit: window
            })
        );
        assert_eq!(credit.reserve(window), Err(Refusal::Wait));
    }

    #[test]
    fn counters_refuse_to_overflow() {
        let mut credit = Credit::starting_at(100, u64::MAX - 50, 0);
        assert_eq!(credit.reserve(50), Ok(0));
        assert_eq!(credit.reserve(1), Err(Refusal::Overflow));
        let mut sequence = Credit::starting_at(100, 0, u64::MAX);
        assert_eq!(sequence.reserve(1), Err(Refusal::Overflow));

        let mut intake = Intake::starting_at(100, u64::MAX - 10, 0);
        assert_eq!(intake.accept(0, 11), Err(IntakeViolation::Overflow));
        assert_eq!(intake.accept(0, 10), Ok(()));
        let mut last = Intake::starting_at(100, 0, u64::MAX);
        assert_eq!(last.accept(u64::MAX, 1), Err(IntakeViolation::Overflow));
    }

    #[test]
    fn gaps_and_reorders_are_detected_not_skipped() {
        let mut intake = Intake::new(1000);
        intake.accept(0, 10).expect("in order");
        assert_eq!(
            intake.accept(2, 10),
            Err(IntakeViolation::Gap {
                expected: 1,
                got: 2
            })
        );
        assert_eq!(
            intake.accept(0, 10),
            Err(IntakeViolation::Reordered {
                expected: 1,
                got: 0
            })
        );
        intake.accept(1, 10).expect("in order");
        assert_eq!(intake.end(2), Ok(()));
        assert_eq!(
            intake.end(3),
            Err(IntakeViolation::CountMismatch {
                announced: 3,
                received: 2
            })
        );
    }

    #[test]
    fn a_producer_ignoring_credit_is_caught_by_the_consumer() {
        let mut intake = Intake::new(100);
        intake.accept(0, 60).expect("fits");
        assert_eq!(
            intake.accept(1, 60),
            Err(IntakeViolation::WindowExceeded {
                buffered: 120,
                window: 100
            })
        );
        // Consumption makes room; bytes read off the socket alone do not.
        assert_eq!(intake.consume(60), 60);
        intake.accept(1, 60).expect("room after consumption");
        assert_eq!(intake.buffered(), 60);
        assert_eq!(intake.items(), 2);
    }
}
