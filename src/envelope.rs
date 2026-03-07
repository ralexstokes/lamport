use std::{
    any::{Any, type_name},
    fmt,
    sync::mpsc,
};

use crate::{
    control::{ControlResult, StateSnapshot, TraceOptions},
    scheduler::PoolKind,
    types::{ActorId, ExitReason, Ref, TimerToken},
};

/// Soft limit for inline payloads stored directly in mailboxes.
///
/// The runtime does not measure payload byte size at send time. Instead, it
/// bounds mailbox growth by envelope count and recommends that callers move
/// larger immutable payloads behind shared buffers such as `Arc<[u8]>`, `Bytes`,
/// or `Arc<str>` once they routinely exceed this size.
pub const RECOMMENDED_INLINE_MESSAGE_LIMIT_BYTES: usize = 64 * 1024;

/// Marker trait for values that can be sent through actor mailboxes.
pub trait Message: Any + Send + 'static {}

impl<T> Message for T where T: Any + Send + 'static {}

/// A type-erased mailbox payload with downcast support.
pub struct Payload {
    inner: Box<dyn Any + Send>,
    type_name: &'static str,
}

impl Payload {
    /// Wraps a typed message for mailbox delivery.
    pub fn new<M: Message>(message: M) -> Self {
        Self {
            inner: Box::new(message),
            type_name: type_name::<M>(),
        }
    }

    /// Returns the Rust type name captured at send time.
    pub const fn type_name(&self) -> &'static str {
        self.type_name
    }

    /// Returns `true` if the payload contains `M`.
    pub fn is<M: Message>(&self) -> bool {
        self.inner.is::<M>()
    }

    /// Borrows the inner message as `M` when the type matches.
    pub fn downcast_ref<M: Message>(&self) -> Option<&M> {
        self.inner.downcast_ref::<M>()
    }

    /// Consumes the payload and returns the inner message when the type matches.
    pub fn downcast<M: Message>(self) -> Result<M, Self> {
        let type_name = self.type_name;

        match self.inner.downcast::<M>() {
            Ok(value) => Ok(*value),
            Err(inner) => Err(Self { inner, type_name }),
        }
    }
}

impl fmt::Debug for Payload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Payload")
            .field("type_name", &self.type_name)
            .finish()
    }
}

/// Reply routing information for synchronous request/response patterns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReplyToken {
    /// The actor waiting for the response.
    pub from: ActorId,
    /// The unique request reference.
    pub reference: Ref,
}

impl ReplyToken {
    /// Creates a reply token for the actor and request reference.
    pub const fn new(from: ActorId, reference: Ref) -> Self {
        Self { from, reference }
    }
}

/// Exit propagation between linked actors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitSignal {
    /// The actor that exited.
    pub from: ActorId,
    /// The reason for the exit.
    pub reason: ExitReason,
    /// Whether the exit came from a bidirectional link.
    pub linked: bool,
}

/// One-way monitor notification sent when an observed actor exits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownMessage {
    /// The monitor reference returned by `monitor`.
    pub reference: Ref,
    /// The actor that terminated.
    pub actor: ActorId,
    /// The exit reason for the terminated actor.
    pub reason: ExitReason,
}

/// Timer delivery used to wake actors after a delay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimerFired {
    /// The timer token that fired.
    pub token: TimerToken,
}

/// Request timeout emitted when an `ask` does not receive a reply in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallTimedOut {
    /// The timed-out request reference.
    pub reference: Ref,
}

/// Blocking task completion routed back into the originating actor mailbox.
#[derive(Debug)]
pub struct TaskCompleted {
    /// The original task handle reference.
    pub reference: Ref,
    /// Pool that executed the task.
    pub pool: PoolKind,
    /// Completed task result payload.
    pub result: Payload,
}

/// Reserved runtime control messages.
#[derive(Debug)]
pub enum SystemMessage {
    /// Suspend user-message execution.
    Suspend,
    /// Resume user-message execution.
    Resume,
    /// Request a state snapshot from the actor.
    GetState {
        /// Reply channel for the control result.
        reply: mpsc::Sender<ControlResult<StateSnapshot>>,
    },
    /// Replace the actor state in a controlled code-change path.
    ReplaceState {
        /// Replacement payload and version.
        state: StateSnapshot,
        /// Reply channel for the control result.
        reply: mpsc::Sender<ControlResult<()>>,
    },
    /// Enable tracing for the actor.
    TraceOn {
        /// Tracing dimensions to enable.
        options: TraceOptions,
        /// Reply channel for the control result.
        reply: mpsc::Sender<ControlResult<()>>,
    },
    /// Disable tracing for the actor.
    TraceOff {
        /// Reply channel for the control result.
        reply: mpsc::Sender<ControlResult<()>>,
    },
    /// Shutdown initiated by the runtime or supervisor.
    Shutdown,
    /// Trigger a code-change hook without carrying the new state directly.
    CodeChange {
        /// Target version requested by the caller.
        target_version: u64,
        /// Reply channel for the control result.
        reply: mpsc::Sender<ControlResult<()>>,
    },
}

/// High-level envelope categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvelopeKind {
    /// Application-level fire-and-forget message.
    User,
    /// Application-level synchronous request.
    Request,
    /// Reply to a prior synchronous request.
    Reply,
    /// Completion of a blocking task submitted by the actor.
    Task,
    /// Request timeout emitted by the runtime.
    CallTimeout,
    /// Exit signal from a linked actor.
    Exit,
    /// Down notification from a monitored actor.
    Down,
    /// Timer message emitted by the runtime.
    Timer,
    /// Reserved runtime control message.
    System,
}

impl EnvelopeKind {
    pub(crate) const fn uses_runtime_reserve(self) -> bool {
        !matches!(self, Self::User | Self::Request)
    }
}

/// A mailbox item delivered to an actor turn.
#[derive(Debug)]
pub enum Envelope {
    /// Fire-and-forget application message.
    User(Payload),
    /// Synchronous request with a reply token.
    Request {
        /// The actor and reference that should receive the reply.
        token: ReplyToken,
        /// The request payload.
        message: Payload,
    },
    /// Reply to a prior request.
    Reply {
        /// The request reference being satisfied.
        reference: Ref,
        /// The reply payload.
        message: Payload,
    },
    /// Completion of a blocking task submitted by the actor.
    Task(TaskCompleted),
    /// Request timeout emitted by the runtime.
    CallTimeout(CallTimedOut),
    /// Exit signal emitted by a linked or runtime-owned relationship.
    Exit(ExitSignal),
    /// Down notification emitted by a monitor.
    Down(DownMessage),
    /// Timer wakeup emitted by the runtime.
    Timer(TimerFired),
    /// Reserved runtime control message.
    System(SystemMessage),
}

impl Envelope {
    /// Creates a user envelope from a typed message.
    pub fn user<M: Message>(message: M) -> Self {
        Self::User(Payload::new(message))
    }

    /// Creates a synchronous request envelope.
    pub fn request<M: Message>(token: ReplyToken, message: M) -> Self {
        Self::Request {
            token,
            message: Payload::new(message),
        }
    }

    /// Creates a reply envelope.
    pub fn reply<M: Message>(reference: Ref, message: M) -> Self {
        Self::Reply {
            reference,
            message: Payload::new(message),
        }
    }

    /// Creates a blocking task completion envelope.
    pub fn task<M: Message>(reference: Ref, pool: PoolKind, message: M) -> Self {
        Self::Task(TaskCompleted {
            reference,
            pool,
            result: Payload::new(message),
        })
    }

    /// Creates a request-timeout envelope.
    pub const fn call_timeout(reference: Ref) -> Self {
        Self::CallTimeout(CallTimedOut { reference })
    }

    /// Returns the envelope category.
    pub const fn kind(&self) -> EnvelopeKind {
        match self {
            Self::User(_) => EnvelopeKind::User,
            Self::Request { .. } => EnvelopeKind::Request,
            Self::Reply { .. } => EnvelopeKind::Reply,
            Self::Task(_) => EnvelopeKind::Task,
            Self::CallTimeout(_) => EnvelopeKind::CallTimeout,
            Self::Exit(_) => EnvelopeKind::Exit,
            Self::Down(_) => EnvelopeKind::Down,
            Self::Timer(_) => EnvelopeKind::Timer,
            Self::System(_) => EnvelopeKind::System,
        }
    }

    pub(crate) const fn is_system_message(&self) -> bool {
        matches!(self, Self::System(_))
    }

    pub(crate) const fn can_run_while_suspended(&self) -> bool {
        matches!(self, Self::System(_) | Self::Exit(_) | Self::Down(_))
    }
}

#[cfg(test)]
mod tests {
    use super::{Envelope, Message};

    #[test]
    fn payload_round_trips_through_downcast() {
        fn extract<M: Message>(envelope: Envelope) -> M {
            match envelope {
                Envelope::User(payload) => payload.downcast::<M>().ok().unwrap(),
                other => panic!("unexpected envelope: {other:?}"),
            }
        }

        assert_eq!(extract::<u32>(Envelope::user(42_u32)), 42);
    }
}
