use crate::{
    lifecycle::{LifecycleEvent, ShutdownPhase},
    types::ExitReason,
};

use super::{RuntimeEvent, RuntimeEventKind};

pub(crate) fn emit_tracing_event(record: &RuntimeEvent) {
    match &record.kind {
        RuntimeEventKind::Lifecycle(event) => emit_lifecycle_event(record, event),
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

fn emit_lifecycle_event(record: &RuntimeEvent, event: &LifecycleEvent) {
    match event {
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
        } => emit_exit(record.sequence, actor, name, reason, parent, ancestors),
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
        } => emit_shutdown(record.sequence, requester, actor, policy, *phase, reason),
    }
}

fn emit_exit(
    sequence: u64,
    actor: &crate::types::ActorId,
    name: &&'static str,
    reason: &ExitReason,
    parent: &Option<crate::types::ActorId>,
    ancestors: &[crate::types::ActorId],
) {
    if matches!(reason, ExitReason::Normal | ExitReason::Shutdown) {
        tracing::event!(
            target: "lamport.lifecycle",
            tracing::Level::INFO,
            sequence,
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
            sequence,
            actor_id = %actor,
            actor_name = *name,
            reason = %reason,
            parent = ?parent,
            ancestors = ?ancestors,
            "actor exited"
        );
    }
}

fn emit_shutdown(
    sequence: u64,
    requester: &crate::types::ActorId,
    actor: &crate::types::ActorId,
    policy: &crate::types::Shutdown,
    phase: ShutdownPhase,
    reason: &Option<ExitReason>,
) {
    match phase {
        ShutdownPhase::TimedOut => {
            tracing::event!(
                target: "lamport.lifecycle",
                tracing::Level::WARN,
                sequence,
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
                sequence,
                requester = %requester,
                actor_id = %actor,
                policy = ?policy,
                phase = ?phase,
                reason = ?reason,
                "actor shutdown lifecycle update"
            );
        }
    }
}
