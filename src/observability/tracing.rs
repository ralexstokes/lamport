use crate::{
    lifecycle::{LifecycleEvent, ShutdownPhase},
    observability::{TraceEvent, TraceEventKind},
    types::ExitReason,
};

use super::{RuntimeEvent, RuntimeEventKind};

macro_rules! emit_actor_exit {
    ($level:expr, $sequence:expr, $actor:expr, $name:expr, $reason:expr, $parent:expr, $ancestors:expr) => {
        tracing::event!(
            target: "lamport.lifecycle",
            $level,
            sequence = $sequence,
            actor_id = %$actor,
            actor_name = *$name,
            reason = %$reason,
            parent = ?$parent,
            ancestors = ?$ancestors,
            "actor exited"
        )
    };
}

macro_rules! emit_shutdown_update {
    ($level:expr, $sequence:expr, $requester:expr, $actor:expr, $policy:expr, $phase:expr, $reason:expr) => {
        tracing::event!(
            target: "lamport.lifecycle",
            $level,
            sequence = $sequence,
            requester = %$requester,
            actor_id = %$actor,
            policy = ?$policy,
            phase = ?$phase,
            reason = ?$reason,
            "actor shutdown lifecycle update"
        )
    };
}

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
        RuntimeEventKind::Trace(event) => emit_trace_event(record.sequence, event),
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
        emit_actor_exit!(
            tracing::Level::INFO,
            sequence,
            actor,
            name,
            reason,
            parent,
            ancestors
        );
    } else {
        emit_actor_exit!(
            tracing::Level::WARN,
            sequence,
            actor,
            name,
            reason,
            parent,
            ancestors
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
        ShutdownPhase::TimedOut => emit_shutdown_update!(
            tracing::Level::WARN,
            sequence,
            requester,
            actor,
            policy,
            phase,
            reason
        ),
        ShutdownPhase::Requested | ShutdownPhase::Completed => emit_shutdown_update!(
            tracing::Level::INFO,
            sequence,
            requester,
            actor,
            policy,
            phase,
            reason
        ),
    }
}

fn emit_trace_event(sequence: u64, event: &TraceEvent) {
    match &event.kind {
        TraceEventKind::Sent { to, envelope_kind } => tracing::event!(
            target: "lamport.trace",
            tracing::Level::DEBUG,
            sequence,
            actor_id = %event.actor,
            actor_name = event.actor_name,
            to = %to,
            envelope_kind = ?envelope_kind,
            mailbox_len = ?event.mailbox_len,
            scheduler_id = ?event.scheduler_id,
            "actor sent envelope"
        ),
        TraceEventKind::Received { envelope_kind } => tracing::event!(
            target: "lamport.trace",
            tracing::Level::DEBUG,
            sequence,
            actor_id = %event.actor,
            actor_name = event.actor_name,
            envelope_kind = ?envelope_kind,
            mailbox_len = ?event.mailbox_len,
            scheduler_id = ?event.scheduler_id,
            "actor received envelope"
        ),
        TraceEventKind::TraceEnabled { options } => tracing::event!(
            target: "lamport.trace",
            tracing::Level::INFO,
            sequence,
            actor_id = %event.actor,
            actor_name = event.actor_name,
            sends = options.sends,
            receives = options.receives,
            mailbox_depth = options.mailbox_depth,
            scheduler = options.scheduler,
            "actor tracing enabled"
        ),
        TraceEventKind::TraceDisabled => tracing::event!(
            target: "lamport.trace",
            tracing::Level::INFO,
            sequence,
            actor_id = %event.actor,
            actor_name = event.actor_name,
            "actor tracing disabled"
        ),
        TraceEventKind::StateInspected { version } => tracing::event!(
            target: "lamport.trace",
            tracing::Level::DEBUG,
            sequence,
            actor_id = %event.actor,
            actor_name = event.actor_name,
            version,
            "actor state inspected"
        ),
        TraceEventKind::StateReplaced { version } => tracing::event!(
            target: "lamport.trace",
            tracing::Level::INFO,
            sequence,
            actor_id = %event.actor,
            actor_name = event.actor_name,
            version,
            "actor state replaced"
        ),
        TraceEventKind::CodeChanged {
            from_version,
            to_version,
        } => tracing::event!(
            target: "lamport.trace",
            tracing::Level::INFO,
            sequence,
            actor_id = %event.actor,
            actor_name = event.actor_name,
            from_version,
            to_version,
            "actor code change completed"
        ),
    }
}
