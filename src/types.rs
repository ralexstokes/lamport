use std::{
    fmt,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
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

/// Exit reasons modeled after the OTP failure protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitReason {
    /// Clean actor completion.
    Normal,
    /// Ordered shutdown from a supervisor or caller.
    Shutdown,
    /// Untrappable kill signal.
    Kill,
    /// Target actor does not exist.
    NoProc,
    /// Target node is unavailable.
    NoConnection,
    /// Arbitrary runtime or application error.
    Error(String),
}

impl fmt::Display for ExitReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Normal => f.write_str("normal"),
            Self::Shutdown => f.write_str("shutdown"),
            Self::Kill => f.write_str("killed"),
            Self::NoProc => f.write_str("no process"),
            Self::NoConnection => f.write_str("no connection"),
            Self::Error(detail) => write!(f, "error: {detail}"),
        }
    }
}

impl std::error::Error for ExitReason {}

/// Identifies an actor in crash reports and topology snapshots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActorIdentity {
    /// Stable runtime id for the actor.
    pub actor: ActorId,
    /// Human-readable actor name.
    pub name: &'static str,
    /// Optional registered name.
    pub registered_name: Option<String>,
}

impl ActorIdentity {
    /// Creates an actor identity from runtime metadata.
    pub fn new(actor: ActorId, name: &'static str, registered_name: Option<String>) -> Self {
        Self {
            actor,
            name,
            registered_name,
        }
    }
}

impl fmt::Display for ActorIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} `{}`", self.actor, self.name)?;
        if let Some(name) = self.registered_name.as_deref() {
            write!(f, " ({name})")?;
        }
        Ok(())
    }
}

/// Restart policy for supervisor children.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restart {
    /// Always restart the child.
    Permanent,
    /// Restart only on abnormal exits.
    Transient,
    /// Never restart the child.
    Temporary,
}

/// Child restart strategy for a supervisor subtree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// Restart only the failed child.
    OneForOne,
    /// Restart all children when any child fails.
    OneForAll,
    /// Restart the failed child and any child started after it.
    RestForOne,
}

/// Shutdown policy used when terminating children.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shutdown {
    /// Terminate immediately without graceful shutdown.
    BrutalKill,
    /// Give the child time to exit before forcing termination.
    Timeout(Duration),
    /// Wait indefinitely for shutdown.
    Infinity,
}

impl Default for Shutdown {
    fn default() -> Self {
        Self::Timeout(Duration::from_secs(5))
    }
}

/// Supervisor-level restart intensity configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupervisorFlags {
    /// The restart strategy for the subtree.
    pub strategy: Strategy,
    /// The maximum restarts allowed inside the period window.
    pub intensity: u32,
    /// The time window used for restart throttling.
    pub period: Duration,
}

impl Default for SupervisorFlags {
    fn default() -> Self {
        Self {
            strategy: Strategy::OneForOne,
            intensity: 3,
            period: Duration::from_secs(5),
        }
    }
}

/// Static child metadata used by supervisors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildSpec {
    /// Stable child identifier within the supervisor.
    pub id: &'static str,
    /// Restart policy for the child.
    pub restart: Restart,
    /// Shutdown behavior when terminating the child.
    pub shutdown: Shutdown,
    /// Whether the child is itself a supervisor.
    pub is_supervisor: bool,
}

impl ChildSpec {
    /// Returns `true` when the child should be restarted for the exit reason.
    pub fn should_restart(&self, reason: &ExitReason) -> bool {
        match self.restart {
            Restart::Permanent => true,
            Restart::Temporary => false,
            Restart::Transient => !matches!(reason, ExitReason::Normal | ExitReason::Shutdown),
        }
    }
}

/// Actor lifecycle state exposed through observability APIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ActorStatus {
    /// The actor has been allocated but not yet initialized.
    #[default]
    Starting,
    /// The actor is ready to run.
    Runnable,
    /// The actor is blocked on a mailbox, timer, or async completion.
    Waiting,
    /// The actor is currently executing on a scheduler thread.
    Running,
    /// The actor is unwinding and running termination hooks.
    Exiting,
    /// The actor has terminated.
    Dead,
}

/// Observable actor metrics useful for an Observer-style UI.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ActorMetrics {
    /// Current mailbox length.
    pub mailbox_len: usize,
    /// Number of actor turns processed.
    pub turns_run: u64,
    /// Number of restarts observed by supervisors.
    pub restarts: u64,
    /// Most recent exit reason, if the actor has terminated before.
    pub last_exit: Option<ExitReason>,
    /// Current scheduler assignment, if runnable.
    pub scheduler_id: Option<usize>,
}

#[cfg(test)]
mod tests {
    use super::{ChildSpec, ExitReason, Restart, Shutdown};

    #[test]
    fn transient_children_restart_only_on_abnormal_exit() {
        let spec = ChildSpec {
            id: "worker",
            restart: Restart::Transient,
            shutdown: Shutdown::default(),
            is_supervisor: false,
        };

        assert!(!spec.should_restart(&ExitReason::Normal));
        assert!(!spec.should_restart(&ExitReason::Shutdown));
        assert!(spec.should_restart(&ExitReason::Kill));
        assert!(spec.should_restart(&ExitReason::Error("boom".into())));
    }
}
