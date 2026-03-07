use std::{
    collections::{BTreeMap, BTreeSet},
    time::SystemTime,
};

use crate::{
    scheduler::{RunQueueSnapshot, SchedulerMetrics},
    snapshot::ActorSnapshot,
    types::{ActorId, ActorStatus},
};

use super::{ActorTree, ActorTreeNode, RuntimeIntrospection, RuntimeMetricsSnapshot};

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
