use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::{self, Write},
    time::SystemTime,
};

use crate::{
    runtime::ActorSnapshot,
    scheduler::{RunQueueSnapshot, SchedulerMetrics},
    types::{ActorId, ActorStatus, CrashReport, LifecycleEvent, ShutdownPhase},
};

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

/// Cursor for incremental runtime event consumption.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EventCursor {
    next_sequence: u64,
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

impl RuntimeMetricsSnapshot {
    /// Encodes the snapshot using Prometheus text exposition format.
    pub fn to_prometheus(&self) -> String {
        let mut output = String::new();
        let observed_at = self
            .observed_at
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();

        let scalars: &[(&str, &str, &str, &dyn fmt::Display)] = &[
            ("lamport_runtime_observed_at_seconds", "Unix timestamp when the runtime metrics snapshot was captured.", "gauge", &observed_at),
            ("lamport_runtime_live_actors", "Current number of live actors in the runtime.", "gauge", &self.live_actors),
            ("lamport_runtime_completed_actors", "Number of completed actor snapshots retained for introspection.", "gauge", &self.completed_actors),
            ("lamport_runtime_mailbox_messages", "Total number of messages queued across live actor mailboxes.", "gauge", &self.total_mailbox_len),
            ("lamport_runtime_mailbox_max_messages", "Largest mailbox length across live actors.", "gauge", &self.max_mailbox_len),
            ("lamport_scheduler_utilization_ratio", "Estimated fraction of busy scheduler time over the runtime lifetime.", "gauge", &self.scheduler_metrics.utilization),
            ("lamport_scheduler_normal_turns_total", "Total actor turns executed on normal schedulers.", "counter", &self.scheduler_metrics.normal_turns),
            ("lamport_scheduler_idle_turns_total", "Total idle polling turns observed by schedulers.", "counter", &self.scheduler_metrics.idle_turns),
            ("lamport_scheduler_blocking_io_jobs_total", "Total jobs submitted to the blocking I/O pool.", "counter", &self.scheduler_metrics.blocking_io_jobs),
            ("lamport_scheduler_blocking_cpu_jobs_total", "Total jobs submitted to the blocking CPU pool.", "counter", &self.scheduler_metrics.blocking_cpu_jobs),
        ];
        for (name, help, metric_type, value) in scalars {
            write_scalar_metric(&mut output, name, help, metric_type, value);
        }

        write_metric_header(
            &mut output,
            "lamport_runtime_actor_status",
            "Number of actors in each lifecycle state.",
            "gauge",
        );
        for (status, value) in [
            ("starting", self.starting_actors),
            ("runnable", self.runnable_actors),
            ("waiting", self.waiting_actors),
            ("running", self.running_actors),
            ("exiting", self.exiting_actors),
            ("dead", self.dead_actors),
        ] {
            write_single_label_metric(
                &mut output,
                "lamport_runtime_actor_status",
                "status",
                status,
                value,
            );
        }

        write_metric_header(
            &mut output,
            "lamport_scheduler_runnable_actors",
            "Number of runnable actors assigned to each scheduler.",
            "gauge",
        );
        write_metric_header(
            &mut output,
            "lamport_scheduler_waiting_actors",
            "Number of waiting actors assigned to each scheduler.",
            "gauge",
        );
        write_metric_header(
            &mut output,
            "lamport_scheduler_injected_total",
            "Actors injected onto each scheduler from the global queue.",
            "counter",
        );
        write_metric_header(
            &mut output,
            "lamport_scheduler_stolen_total",
            "Actors stolen by each scheduler.",
            "counter",
        );

        let mut run_queues = self.run_queues.clone();
        run_queues.sort_by_key(|snapshot| snapshot.scheduler_id);

        for snapshot in run_queues {
            write_single_label_metric(
                &mut output,
                "lamport_scheduler_runnable_actors",
                "scheduler_id",
                snapshot.scheduler_id,
                snapshot.runnable,
            );
            write_single_label_metric(
                &mut output,
                "lamport_scheduler_waiting_actors",
                "scheduler_id",
                snapshot.scheduler_id,
                snapshot.waiting,
            );
            write_single_label_metric(
                &mut output,
                "lamport_scheduler_injected_total",
                "scheduler_id",
                snapshot.scheduler_id,
                snapshot.injected,
            );
            write_single_label_metric(
                &mut output,
                "lamport_scheduler_stolen_total",
                "scheduler_id",
                snapshot.scheduler_id,
                snapshot.stolen,
            );
        }

        output
    }
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

pub(crate) fn build_actor_tree(actors: &[ActorSnapshot]) -> ActorTree {
    let mut actors = actors.to_vec();
    actors.sort_by_key(|snapshot| snapshot.id);
    let live_ids: BTreeSet<_> = actors.iter().map(|snapshot| snapshot.id).collect();
    let mut children_by_parent: BTreeMap<ActorId, Vec<ActorId>> = BTreeMap::new();
    let mut roots = Vec::new();

    for actor in &actors {
        match actor.parent.filter(|parent| live_ids.contains(parent)) {
            Some(parent) => children_by_parent.entry(parent).or_default().push(actor.id),
            None => roots.push(actor.id),
        }
    }

    roots.sort_unstable();

    let nodes = actors
        .into_iter()
        .map(|actor| {
            let mut children = children_by_parent.remove(&actor.id).unwrap_or_default();
            children.sort_unstable();
            ActorTreeNode { actor, children }
        })
        .collect();

    ActorTree { roots, nodes }
}

pub(crate) fn build_metrics_snapshot(
    actors: &[ActorSnapshot],
    completed_actors: usize,
    scheduler_metrics: SchedulerMetrics,
    run_queues: Vec<RunQueueSnapshot>,
) -> RuntimeMetricsSnapshot {
    let mut starting_actors = 0;
    let mut runnable_actors = 0;
    let mut waiting_actors = 0;
    let mut running_actors = 0;
    let mut exiting_actors = 0;
    let mut dead_actors = 0;
    let mut total_mailbox_len = 0;
    let mut max_mailbox_len = 0;

    for actor in actors {
        total_mailbox_len += actor.metrics.mailbox_len;
        max_mailbox_len = max_mailbox_len.max(actor.metrics.mailbox_len);
        match actor.status {
            ActorStatus::Starting => starting_actors += 1,
            ActorStatus::Runnable => runnable_actors += 1,
            ActorStatus::Waiting => waiting_actors += 1,
            ActorStatus::Running => running_actors += 1,
            ActorStatus::Exiting => exiting_actors += 1,
            ActorStatus::Dead => dead_actors += 1,
        }
    }

    RuntimeMetricsSnapshot {
        observed_at: SystemTime::now(),
        live_actors: actors.len(),
        completed_actors,
        starting_actors,
        runnable_actors,
        waiting_actors,
        running_actors,
        exiting_actors,
        dead_actors,
        total_mailbox_len,
        max_mailbox_len,
        scheduler_metrics,
        run_queues,
    }
}

pub(crate) fn build_runtime_introspection(
    actors: Vec<ActorSnapshot>,
    completed_actors: usize,
    scheduler_metrics: SchedulerMetrics,
    run_queues: Vec<RunQueueSnapshot>,
) -> RuntimeIntrospection {
    let actor_tree = build_actor_tree(&actors);
    let metrics = build_metrics_snapshot(&actors, completed_actors, scheduler_metrics, run_queues);

    RuntimeIntrospection {
        actors,
        actor_tree,
        metrics,
    }
}

pub(crate) fn emit_tracing_event(record: &RuntimeEvent) {
    match &record.kind {
        RuntimeEventKind::Lifecycle(event) => match event {
            LifecycleEvent::Spawn {
                actor,
                name,
                registered_name,
                parent,
                supervisor_child,
            } => {
                tracing::event!(
                    target: "lamport.lifecycle",
                    tracing::Level::INFO,
                    sequence = record.sequence,
                    actor_id = %actor,
                    actor_name = *name,
                    registered_name = registered_name.as_deref().unwrap_or(""),
                    parent = ?parent,
                    supervisor_child = supervisor_child.unwrap_or(""),
                    "actor spawned"
                );
            }
            LifecycleEvent::Exit {
                actor,
                name,
                reason,
                parent,
                ancestors,
            } => {
                if matches!(
                    reason,
                    crate::types::ExitReason::Normal | crate::types::ExitReason::Shutdown
                ) {
                    tracing::event!(
                        target: "lamport.lifecycle",
                        tracing::Level::INFO,
                        sequence = record.sequence,
                        actor_id = %actor,
                        actor_name = *name,
                        reason = %reason,
                        parent = ?parent,
                        ancestors = ?ancestors,
                        "actor exited"
                    );
                } else {
                    tracing::event!(
                        target: "lamport.lifecycle",
                        tracing::Level::WARN,
                        sequence = record.sequence,
                        actor_id = %actor,
                        actor_name = *name,
                        reason = %reason,
                        parent = ?parent,
                        ancestors = ?ancestors,
                        "actor exited"
                    );
                }
            }
            LifecycleEvent::Down {
                watcher,
                actor,
                reference,
                reason,
            } => {
                tracing::event!(
                    target: "lamport.lifecycle",
                    tracing::Level::INFO,
                    sequence = record.sequence,
                    watcher = %watcher,
                    actor_id = %actor,
                    reference = reference.get(),
                    reason = %reason,
                    "monitor delivered DOWN"
                );
            }
            LifecycleEvent::Restart {
                supervisor,
                child_id,
                old_actor,
                new_actor,
            } => {
                tracing::event!(
                    target: "lamport.lifecycle",
                    tracing::Level::INFO,
                    sequence = record.sequence,
                    supervisor = %supervisor,
                    child_id = *child_id,
                    old_actor = ?old_actor,
                    new_actor = %new_actor,
                    "supervisor restarted child"
                );
            }
            LifecycleEvent::Shutdown {
                requester,
                actor,
                policy,
                phase,
                reason,
            } => match phase {
                ShutdownPhase::TimedOut => {
                    tracing::event!(
                        target: "lamport.lifecycle",
                        tracing::Level::WARN,
                        sequence = record.sequence,
                        requester = %requester,
                        actor_id = %actor,
                        policy = ?policy,
                        phase = ?phase,
                        reason = ?reason,
                        "actor shutdown lifecycle update"
                    );
                }
                ShutdownPhase::Requested | ShutdownPhase::Completed => {
                    tracing::event!(
                        target: "lamport.lifecycle",
                        tracing::Level::INFO,
                        sequence = record.sequence,
                        requester = %requester,
                        actor_id = %actor,
                        policy = ?policy,
                        phase = ?phase,
                        reason = ?reason,
                        "actor shutdown lifecycle update"
                    );
                }
            },
        },
        RuntimeEventKind::Crash(report) => {
            tracing::event!(
                target: "lamport.crash",
                tracing::Level::ERROR,
                sequence = record.sequence,
                actor_id = %report.actor,
                actor_name = report.name,
                reason = %report.reason,
                parent = ?report.parent,
                ancestors = ?report.ancestors,
                crash_report = %report,
                "actor crashed"
            );
        }
    }
}

pub(crate) fn events_since(events: &[RuntimeEvent], cursor: &mut EventCursor) -> Vec<RuntimeEvent> {
    let start = events.partition_point(|e| e.sequence < cursor.next_sequence);
    let pending = events[start..].to_vec();
    if let Some(last) = pending.last() {
        cursor.next_sequence = last.sequence + 1;
    }
    pending
}

fn write_metric_header(output: &mut String, name: &str, help: &str, metric_type: &str) {
    writeln!(output, "# HELP {name} {help}").unwrap();
    writeln!(output, "# TYPE {name} {metric_type}").unwrap();
}

fn write_scalar_metric(
    output: &mut String,
    name: &str,
    help: &str,
    metric_type: &str,
    value: impl fmt::Display,
) {
    write_metric_header(output, name, help, metric_type);
    writeln!(output, "{name} {value}").unwrap();
}

fn write_single_label_metric(
    output: &mut String,
    name: &str,
    label_name: &str,
    label_value: impl fmt::Display,
    value: impl fmt::Display,
) {
    writeln!(output, "{name}{{{label_name}=\"{label_value}\"}} {value}").unwrap();
}
