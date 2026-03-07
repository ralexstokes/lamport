mod prometheus;
mod types;

pub use types::{
    ActorTree, ActorTreeNode, EventCursor, RuntimeEvent, RuntimeEventKind, RuntimeIntrospection,
    RuntimeMetricsSnapshot,
};

use std::{
    collections::{BTreeMap, BTreeSet},
    time::SystemTime,
};

use crate::{
    lifecycle::{LifecycleEvent, ShutdownPhase},
    scheduler::{RunQueueSnapshot, SchedulerMetrics},
    snapshot::ActorSnapshot,
    types::{ActorId, ActorStatus, ExitReason},
};

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
                if matches!(reason, ExitReason::Normal | ExitReason::Shutdown) {
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
