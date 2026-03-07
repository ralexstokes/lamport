use std::time::SystemTime;

use crate::{
    control::TraceOptions,
    envelope::EnvelopeKind,
    lifecycle::{CrashReport, LifecycleEvent},
    scheduler::{RunQueueSnapshot, SchedulerMetrics},
    snapshot::ActorSnapshot,
    types::ActorId,
};

/// Cursor for incremental runtime event consumption.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EventCursor {
    pub(crate) next_sequence: u64,
}

impl EventCursor {
    /// Creates a cursor positioned at the start of the retained event log.
    pub const fn from_start() -> Self {
        Self { next_sequence: 0 }
    }

    /// Returns the next sequence number that will be requested.
    pub const fn next_sequence(self) -> u64 {
        self.next_sequence
    }

    pub(crate) const fn new(next_sequence: u64) -> Self {
        Self { next_sequence }
    }
}

/// Structured runtime event suitable for tracing, debugging, and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeEvent {
    /// Monotonic sequence number within a runtime instance.
    pub sequence: u64,
    /// Wall-clock timestamp when the event was emitted.
    pub emitted_at: SystemTime,
    /// Typed runtime event payload.
    pub kind: RuntimeEventKind,
}

/// Typed runtime event variants emitted by the observability layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeEventKind {
    /// Lifecycle transition emitted by the runtime.
    Lifecycle(LifecycleEvent),
    /// Abnormal crash report emitted for an actor exit.
    Crash(CrashReport),
    /// Actor-scoped trace event emitted when tracing is enabled.
    Trace(TraceEvent),
}

/// Actor-scoped trace event captured through the control plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceEvent {
    /// Actor producing or receiving the traceable action.
    pub actor: ActorId,
    /// Human-readable actor name.
    pub actor_name: &'static str,
    /// Trace payload.
    pub kind: TraceEventKind,
    /// Optional mailbox depth captured for the event.
    pub mailbox_len: Option<usize>,
    /// Optional scheduler id captured for the event.
    pub scheduler_id: Option<usize>,
}

/// Supported trace payload variants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceEventKind {
    /// Actor sent an envelope to another actor.
    Sent {
        /// Destination actor.
        to: ActorId,
        /// Envelope kind sent by the actor.
        envelope_kind: EnvelopeKind,
    },
    /// Actor received an envelope from its mailbox.
    Received {
        /// Envelope kind delivered to the actor.
        envelope_kind: EnvelopeKind,
    },
    /// Actor tracing was enabled with the given options.
    TraceEnabled {
        /// Activated trace dimensions.
        options: TraceOptions,
    },
    /// Actor tracing was disabled.
    TraceDisabled,
    /// State inspection completed successfully.
    StateInspected {
        /// Current actor state version.
        version: u64,
    },
    /// State replacement completed successfully.
    StateReplaced {
        /// Current actor state version.
        version: u64,
    },
    /// Reserved code change completed successfully.
    CodeChanged {
        /// Previous actor state version.
        from_version: u64,
        /// Requested target version.
        to_version: u64,
    },
}

/// Parent-child actor relationships for observer-style UIs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActorTreeNode {
    /// Actor snapshot for the node.
    pub actor: ActorSnapshot,
    /// Direct children in the supervision tree.
    pub children: Vec<ActorId>,
}

/// Runtime-wide actor tree snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActorTree {
    /// Root actors with no live parent in the snapshot.
    pub roots: Vec<ActorId>,
    /// Nodes ordered by actor id.
    pub nodes: Vec<ActorTreeNode>,
}

/// Aggregated runtime metrics safe to export to production monitoring.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeMetricsSnapshot {
    /// Wall-clock capture time for the snapshot.
    pub observed_at: SystemTime,
    /// Number of live actors.
    pub live_actors: usize,
    /// Number of completed actors retained by the runtime.
    pub completed_actors: usize,
    /// Actors currently starting.
    pub starting_actors: usize,
    /// Actors ready to run.
    pub runnable_actors: usize,
    /// Actors waiting on work.
    pub waiting_actors: usize,
    /// Actors currently running.
    pub running_actors: usize,
    /// Actors exiting.
    pub exiting_actors: usize,
    /// Actors retained as dead snapshots.
    pub dead_actors: usize,
    /// Sum of live mailbox lengths.
    pub total_mailbox_len: usize,
    /// Largest live mailbox length.
    pub max_mailbox_len: usize,
    /// Aggregate scheduler metrics.
    pub scheduler_metrics: SchedulerMetrics,
    /// Per-scheduler queue snapshots.
    pub run_queues: Vec<RunQueueSnapshot>,
}

/// Runtime-wide snapshot combining topology and monitoring metrics.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeIntrospection {
    /// Live actor snapshots with mailbox, link, and monitor metadata.
    pub actors: Vec<ActorSnapshot>,
    /// Parent-child actor tree for observer-style UIs.
    pub actor_tree: ActorTree,
    /// Aggregated runtime monitoring metrics.
    pub metrics: RuntimeMetricsSnapshot,
}
