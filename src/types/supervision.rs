use std::time::Duration;

use super::ExitReason;

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
