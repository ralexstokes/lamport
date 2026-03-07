use super::ExitReason;

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
