use std::fmt;

use crate::{
    context::SendError,
    envelope::{Message, Payload},
    types::ActorId,
};

/// Result type returned by control-plane operations.
pub type ControlResult<T> = Result<T, ControlError>;

/// Type-erased actor state returned by the control plane.
#[derive(Debug)]
pub struct StateSnapshot {
    /// Actor-defined state schema version.
    pub version: u64,
    /// Type-erased state payload.
    pub payload: Payload,
}

impl StateSnapshot {
    /// Creates a new versioned state snapshot.
    pub fn new<M: Message>(version: u64, payload: M) -> Self {
        Self {
            version,
            payload: Payload::new(payload),
        }
    }

    /// Creates a new versioned state snapshot from an existing payload.
    pub const fn from_payload(version: u64, payload: Payload) -> Self {
        Self { version, payload }
    }
}

/// Actor-local tracing options controlled through the reserved control lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TraceOptions {
    /// Emit trace events when the actor sends envelopes.
    pub sends: bool,
    /// Emit trace events when the actor receives envelopes.
    pub receives: bool,
    /// Include the actor's mailbox depth in trace events.
    pub mailbox_depth: bool,
    /// Include the scheduler id in trace events.
    pub scheduler: bool,
}

impl TraceOptions {
    /// Enables send and receive tracing.
    pub const fn messages() -> Self {
        Self {
            sends: true,
            receives: true,
            mailbox_depth: false,
            scheduler: false,
        }
    }

    /// Enables all currently supported tracing dimensions.
    pub const fn all() -> Self {
        Self {
            sends: true,
            receives: true,
            mailbox_depth: true,
            scheduler: true,
        }
    }

    /// Returns `true` when any trace dimension is enabled.
    pub const fn is_enabled(self) -> bool {
        self.sends || self.receives || self.mailbox_depth || self.scheduler
    }
}

/// Structured control-plane failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlError {
    /// The target actor does not exist.
    NoProc(ActorId),
    /// The actor does not support the requested control operation.
    Unsupported {
        /// Reserved control-plane operation name.
        operation: &'static str,
    },
    /// The supplied state version does not match the actor's current version.
    VersionMismatch {
        /// Actor's current version.
        current: u64,
        /// Requested or supplied version.
        requested: u64,
    },
    /// The supplied state payload had the wrong type.
    InvalidState {
        /// Expected Rust type name.
        expected: &'static str,
        /// Actual payload type name.
        actual: &'static str,
    },
    /// The actor rejected the control operation for a domain-specific reason.
    Rejected {
        /// Reserved control-plane operation name.
        operation: &'static str,
        /// Human-readable rejection reason.
        reason: String,
    },
}

impl ControlError {
    /// Creates an unsupported-operation error.
    pub const fn unsupported(operation: &'static str) -> Self {
        Self::Unsupported { operation }
    }

    /// Creates a state-type mismatch error.
    pub const fn invalid_state(expected: &'static str, actual: &'static str) -> Self {
        Self::InvalidState { expected, actual }
    }

    /// Creates an operation rejection with a free-form reason.
    pub fn rejected(operation: &'static str, reason: impl Into<String>) -> Self {
        Self::Rejected {
            operation,
            reason: reason.into(),
        }
    }
}

impl fmt::Display for ControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoProc(actor) => write!(f, "actor `{actor}` does not exist"),
            Self::Unsupported { operation } => {
                write!(f, "actor does not support control operation `{operation}`")
            }
            Self::VersionMismatch { current, requested } => write!(
                f,
                "control state version mismatch: current={current}, requested={requested}"
            ),
            Self::InvalidState { expected, actual } => write!(
                f,
                "invalid replacement state payload: expected `{expected}`, got `{actual}`"
            ),
            Self::Rejected { operation, reason } => {
                write!(f, "control operation `{operation}` rejected: {reason}")
            }
        }
    }
}

/// Runtime stage for a local supervisor-tree upgrade transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalUpgradeStage {
    /// Quiescing normal user-envelope processing by suspending the tree.
    Suspend,
    /// Running actor-local code-change hooks in upgrade order.
    CodeChange,
    /// Releasing the tree back to normal user-envelope processing.
    Resume,
}

/// Failure kind surfaced by a local supervisor-tree upgrade transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalUpgradeFailure {
    /// The requested root is not a live supervisor.
    NotLiveSupervisor(ActorId),
    /// The runtime found a tree-shape invariant that makes upgrade unsafe.
    InvalidTree {
        /// Actor whose subtree failed validation.
        actor: ActorId,
        /// Human-readable rejection reason.
        reason: String,
    },
    /// A reserved suspend or resume message could not be enqueued.
    Send(SendError),
    /// A reserved code-change operation rejected or failed.
    Control(ControlError),
}

/// Successful local supervisor-tree upgrade details.
///
/// Local supervisor-tree upgrades are atomic only with respect to quiescing and
/// resuming normal message processing. Once an actor's `CodeChange` hook
/// succeeds, that actor stays on the requested version for the rest of the
/// transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalUpgradeReport {
    /// Root supervisor that scoped the upgrade.
    pub root: ActorId,
    /// Target version requested for each upgraded actor.
    pub target_version: u64,
    /// Root-to-leaf traversal order used when quiescing the tree.
    pub suspend_order: Vec<ActorId>,
    /// Leaf-to-root traversal order used for `CodeChange`.
    pub upgrade_order: Vec<ActorId>,
}

/// Structured failure returned by a local supervisor-tree upgrade transaction.
///
/// `LocalRuntime` v0 uses resume-only failure semantics: it best-effort resumes
/// every actor suspended by the transaction before returning this error, but it
/// does not roll back actors listed in [`Self::upgraded`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalUpgradeError {
    /// Root supervisor that scoped the upgrade.
    pub root: ActorId,
    /// Target version requested for the upgrade.
    pub target_version: u64,
    /// Actor whose step failed, when the failure happened after planning.
    pub actor: Option<ActorId>,
    /// Upgrade stage that failed, when the failure happened after planning.
    pub stage: Option<LocalUpgradeStage>,
    /// Concrete reason the transaction aborted.
    pub failure: Box<LocalUpgradeFailure>,
    /// Actors newly suspended by this transaction before it failed.
    ///
    /// The runtime attempts to resume these actors before returning the error.
    pub suspended: Vec<ActorId>,
    /// Actors whose `CodeChange` hook completed before the failure.
    ///
    /// These actors remain at `target_version`; partial local upgrades are not
    /// rolled back automatically.
    pub upgraded: Vec<ActorId>,
}

impl fmt::Display for LocalUpgradeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.stage, &self.actor, self.failure.as_ref()) {
            (Some(stage), Some(actor), LocalUpgradeFailure::Send(error)) => {
                write!(
                    f,
                    "local upgrade failed during {stage:?} for actor `{actor}`: {error:?}"
                )
            }
            (Some(stage), Some(actor), LocalUpgradeFailure::Control(error)) => {
                write!(
                    f,
                    "local upgrade failed during {stage:?} for actor `{actor}`: {error}"
                )
            }
            (_, _, LocalUpgradeFailure::NotLiveSupervisor(actor)) => {
                write!(f, "actor `{actor}` is not a live supervisor root")
            }
            (_, _, LocalUpgradeFailure::InvalidTree { actor, reason }) => {
                write!(f, "invalid supervisor tree at actor `{actor}`: {reason}")
            }
            (_, _, LocalUpgradeFailure::Send(error)) => {
                write!(f, "local upgrade failed: {error:?}")
            }
            (_, _, LocalUpgradeFailure::Control(error)) => {
                write!(f, "local upgrade failed: {error}")
            }
        }
    }
}
