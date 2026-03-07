use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
};

use tokio::sync::Notify;

use crate::{
    actor::ActorTurn,
    control::TraceOptions,
    envelope::Envelope,
    internal::panic_reason,
    types::{ActorId, ActorStatus, ExitReason},
};

use super::{
    ActorCell, ConcurrentContext, ReservedActorMetadata, ReservedSystemResult, RuntimeShared,
    apply_system_message_effects, handle_reserved_system_message, maybe_trace_receive,
    take_next_envelope,
};

enum Either {
    Exit(ExitReason),
    Envelope {
        envelope: Envelope,
        name: &'static str,
        scheduler_id: usize,
        mailbox_len: usize,
        trace_options: TraceOptions,
    },
    Wait(Arc<Notify>),
}

pub(super) async fn run_actor_task(
    runtime: Arc<RuntimeShared>,
    actor_id: ActorId,
    mut actor: Box<dyn ActorCell>,
) {
    if let Some(reason) = run_init(&runtime, actor_id, actor.as_mut()) {
        finish_actor_task(runtime, actor_id, actor, reason).await;
        return;
    }

    let mut budget = 0_u32;

    loop {
        let next = {
            let mut state = runtime.lock_state();
            let Some(entry) = RuntimeShared::actor_mut(&mut state, actor_id) else {
                return;
            };

            if let Some(reason) = entry.forced_exit.take() {
                entry.status = ActorStatus::Exiting;
                entry.metrics.scheduler_id = Some(entry.scheduler_id);
                runtime.idle_cv.notify_all();
                Either::Exit(reason)
            } else if let Some(envelope) = take_next_envelope(entry) {
                apply_system_message_effects(entry, &envelope);
                entry.metrics.mailbox_len = entry.mailbox.len();
                entry.status = ActorStatus::Running;
                entry.metrics.scheduler_id = Some(entry.scheduler_id);
                entry.metrics.turns_run += 1;
                let name = entry.name;
                let scheduler_id = entry.scheduler_id;
                let mailbox_len = entry.mailbox.len();
                let trace_options = entry.trace_options;
                state.metrics.normal_turns += 1;
                Either::Envelope {
                    envelope,
                    name,
                    scheduler_id,
                    mailbox_len,
                    trace_options,
                }
            } else {
                entry.status = ActorStatus::Waiting;
                entry.metrics.scheduler_id = None;
                entry.metrics.mailbox_len = entry.mailbox.len();
                let notify = Arc::clone(&entry.notify);
                runtime.idle_cv.notify_all();
                Either::Wait(notify)
            }
        };

        match next {
            Either::Exit(reason) => {
                finish_actor_task(runtime, actor_id, actor, reason).await;
                return;
            }
            Either::Envelope {
                envelope,
                name,
                scheduler_id,
                mailbox_len,
                trace_options,
            } => {
                if trace_options.receives {
                    maybe_trace_receive(
                        &runtime,
                        actor_id,
                        name,
                        trace_options,
                        mailbox_len,
                        scheduler_id,
                        &envelope,
                    );
                }

                let (turn_result, effects) = {
                    let envelope_kind = envelope.kind();
                    let mut ctx = ConcurrentContext::new(Arc::clone(&runtime), actor_id);
                    let turn_span = tracing::trace_span!(
                        "lamport.actor.turn",
                        actor_id = %actor_id,
                        actor_name = name,
                        scheduler_id,
                        envelope_kind = ?envelope_kind
                    );
                    let _turn_guard = turn_span.enter();
                    let result = catch_unwind(AssertUnwindSafe(|| {
                        if let Envelope::System(message) = envelope {
                            match handle_reserved_system_message(
                                &runtime,
                                ReservedActorMetadata {
                                    actor_id,
                                    actor_name: name,
                                    mailbox_len,
                                    scheduler_id,
                                },
                                actor.as_mut(),
                                message,
                                &mut ctx,
                            ) {
                                ReservedSystemResult::Handled(turn) => turn,
                                ReservedSystemResult::Forward(message) => {
                                    actor.handle(Envelope::System(message), &mut ctx)
                                }
                            }
                        } else {
                            actor.handle(envelope, &mut ctx)
                        }
                    }));
                    let effects = ctx.finish();
                    (result, effects)
                };

                let exit_reason = match turn_result {
                    Ok(turn) => effects.exit_reason.or(match turn {
                        ActorTurn::Stop(reason) => Some(reason),
                        ActorTurn::Continue | ActorTurn::Yield => None,
                    }),
                    Err(panic) => Some(panic_reason(actor_id, name, panic)),
                };

                if let Some(reason) = exit_reason {
                    finish_actor_task(runtime, actor_id, actor, reason).await;
                    return;
                }

                {
                    let mut state = runtime.lock_state();
                    if let Some(entry) = RuntimeShared::actor_mut(&mut state, actor_id) {
                        entry.metrics.mailbox_len = entry.mailbox.len();
                        entry.metrics.scheduler_id = None;
                        if entry.mailbox.is_empty() {
                            entry.status = ActorStatus::Waiting;
                        } else {
                            entry.status = ActorStatus::Runnable;
                        }
                    }
                }
                runtime.idle_cv.notify_all();

                budget += 1;
                if effects.yielded || budget >= runtime.config.actor_turn_budget.max(1) {
                    budget = 0;
                    tokio::task::yield_now().await;
                }
            }
            Either::Wait(notify) => {
                notify.notified().await;
            }
        }
    }
}

fn run_init(
    runtime: &Arc<RuntimeShared>,
    actor_id: ActorId,
    actor: &mut dyn ActorCell,
) -> Option<ExitReason> {
    let (name, scheduler_id) = {
        let mut state = runtime.lock_state();
        let Some(entry) = RuntimeShared::actor_mut(&mut state, actor_id) else {
            return Some(ExitReason::Error(format!(
                "actor `{actor_id}` disappeared before init"
            )));
        };
        entry.status = ActorStatus::Running;
        entry.metrics.scheduler_id = Some(entry.scheduler_id);
        (entry.name, entry.scheduler_id)
    };

    let (init_result, effects) = {
        let mut ctx = ConcurrentContext::new(runtime.clone(), actor_id);
        let init_span = tracing::debug_span!(
            "lamport.actor.init",
            actor_id = %actor_id,
            actor_name = name,
            scheduler_id
        );
        let _init_guard = init_span.enter();
        let result = catch_unwind(AssertUnwindSafe(|| actor.init(&mut ctx)));
        let effects = ctx.finish();
        (result, effects)
    };

    match init_result {
        Ok(Ok(())) => {
            let mut state = runtime.lock_state();
            if let Some(entry) = RuntimeShared::actor_mut(&mut state, actor_id) {
                entry.initialized = true;
                entry.metrics.scheduler_id = None;
                if effects.exit_reason.is_some() {
                    entry.status = ActorStatus::Exiting;
                } else if entry.mailbox.is_empty() {
                    entry.status = ActorStatus::Waiting;
                } else {
                    entry.status = ActorStatus::Runnable;
                }
            }
            runtime.idle_cv.notify_all();
            effects.exit_reason
        }
        Ok(Err(reason)) => Some(reason),
        Err(panic) => Some(panic_reason(actor_id, name, panic)),
    }
}

async fn finish_actor_task(
    runtime: Arc<RuntimeShared>,
    actor_id: ActorId,
    mut actor: Box<dyn ActorCell>,
    reason: ExitReason,
) {
    let name = {
        let mut state = runtime.lock_state();
        let Some(entry) = RuntimeShared::actor_mut(&mut state, actor_id) else {
            return;
        };
        entry.status = ActorStatus::Exiting;
        entry.metrics.scheduler_id = Some(entry.scheduler_id);
        entry.name
    };

    let terminate_result = {
        let mut ctx = ConcurrentContext::new(Arc::clone(&runtime), actor_id);
        let terminate_span = tracing::debug_span!(
            "lamport.actor.terminate",
            actor_id = %actor_id,
            actor_name = name,
            reason = %reason
        );
        let _terminate_guard = terminate_span.enter();
        let result = catch_unwind(AssertUnwindSafe(|| {
            actor.terminate(reason.clone(), &mut ctx)
        }));
        let _ = ctx.finish();
        result
    };

    let final_reason = match terminate_result {
        Ok(()) => reason,
        Err(panic) => panic_reason(actor_id, name, panic),
    };

    runtime.finalize_actor(actor_id, name, final_reason);
}
