use std::num::NonZeroUsize;

use crate::{
    actor::Actor,
    context::{SpawnError, SpawnOptions},
    types::ActorId,
};

/// Scheduler pool types for normal and dirty work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PoolKind {
    /// Normal actor turns on the main schedulers.
    Normal,
    /// Blocking I/O work that must not stall the main schedulers.
    BlockingIo,
    /// CPU-heavy work that must not monopolize actor turns.
    BlockingCpu,
}

/// Static scheduler configuration for a runtime instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulerConfig {
    /// Number of normal scheduler threads.
    pub scheduler_count: usize,
    /// Maximum number of live actors allowed in the runtime.
    pub max_actors: usize,
    /// Default bounded mailbox capacity for newly spawned actors.
    pub default_mailbox_capacity: usize,
    /// Slots reserved for runtime-originated envelopes such as replies and timeouts.
    pub mailbox_runtime_reserve: usize,
    /// Maximum entries per local run queue before backpressure is needed.
    pub local_run_queue_capacity: usize,
    /// Maximum entries in the global inject queue.
    pub inject_queue_capacity: usize,
    /// Number of actors to steal at a time from another scheduler.
    pub steal_batch_size: usize,
    /// Maximum actor turns before a scheduler rechecks global work.
    pub actor_turn_budget: u32,
    /// Dedicated dirty I/O worker threads.
    pub blocking_io_threads: usize,
    /// Dedicated dirty CPU worker threads.
    pub blocking_cpu_threads: usize,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        let scheduler_count = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);

        Self {
            scheduler_count,
            max_actors: 65_536,
            default_mailbox_capacity: 1_024,
            mailbox_runtime_reserve: 32,
            local_run_queue_capacity: 4096,
            inject_queue_capacity: 16384,
            steal_batch_size: 64,
            actor_turn_budget: 64,
            blocking_io_threads: scheduler_count.max(4),
            blocking_cpu_threads: scheduler_count.max(2),
        }
    }
}

/// Observable run-queue state for one scheduler thread.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunQueueSnapshot {
    /// Scheduler identifier.
    pub scheduler_id: usize,
    /// Number of runnable actors.
    pub runnable: usize,
    /// Number of waiting actors owned by the scheduler.
    pub waiting: usize,
    /// Number of actors received through the inject queue.
    pub injected: u64,
    /// Number of actors stolen from other schedulers.
    pub stolen: u64,
}

/// Aggregate runtime metrics for Observer-style dashboards.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SchedulerMetrics {
    /// Ratio of busy to total scheduler time in the sample window.
    pub utilization: f32,
    /// Actor turns executed on normal schedulers.
    pub normal_turns: u64,
    /// Idle polling turns.
    pub idle_turns: u64,
    /// Jobs dispatched to the dirty I/O pool.
    pub blocking_io_jobs: u64,
    /// Jobs dispatched to the dirty CPU pool.
    pub blocking_cpu_jobs: u64,
}

/// Enqueue or wakeup failure for the scheduler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleError {
    /// The actor does not exist.
    NoProc(ActorId),
    /// The target queue is full.
    QueueFull {
        /// Scheduler identifier for the full queue.
        scheduler_id: usize,
        /// The actor that could not be enqueued.
        actor: ActorId,
    },
}

/// Runtime scheduler contract for actor placement and wakeups.
pub trait Scheduler: Send + Sync + 'static {
    /// Returns the static scheduler configuration.
    fn config(&self) -> &SchedulerConfig;

    /// Returns the number of normal scheduler threads.
    fn scheduler_count(&self) -> usize {
        self.config().scheduler_count
    }

    /// Spawns a new actor into the runtime.
    fn spawn<A: Actor>(&self, actor: A, options: SpawnOptions) -> Result<ActorId, SpawnError>;

    /// Enqueues an actor for normal execution.
    fn enqueue(&self, actor: ActorId) -> Result<(), ScheduleError>;

    /// Wakes an actor that was waiting on a mailbox, timer, or async completion.
    fn wake(&self, actor: ActorId) -> Result<(), ScheduleError>;

    /// Marks an actor as parked or waiting.
    fn park(&self, actor: ActorId);

    /// Returns run-queue snapshots for introspection.
    fn run_queue_snapshots(&self) -> Vec<RunQueueSnapshot>;

    /// Returns aggregate scheduler metrics.
    fn metrics(&self) -> SchedulerMetrics;
}
