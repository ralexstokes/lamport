use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::{
    context::SendError,
    envelope::{DownMessage, Envelope, ExitSignal},
    internal::{
        ExitDisposition, classify_exit_signal, mailbox_overflow_reason, panic_reason,
        shutdown_link_reason,
    },
    lifecycle::{CrashReport, LifecycleEvent, ShutdownPhase},
    types::{ActorId, ActorStatus, ExitReason, Ref},
};

use super::{ActorEntry, LocalContext, LocalRuntime, park_actor, push_envelope, schedule_actor};

impl LocalRuntime {
    pub(super) fn run_init(&mut self, mut entry: ActorEntry) {
        entry.state.status = ActorStatus::Running;
        entry.state.metrics.scheduler_id = Some(0);

        let (init_result, effects) = {
            let actor_id = entry.state.id;
            let actor_name = entry.name;
            let mut ctx = LocalContext::new(self, &mut entry.state, entry.name);
            let init_span = tracing::debug_span!(
                "lamport.actor.init",
                actor_id = %actor_id,
                actor_name = actor_name,
                scheduler_id = 0usize
            );
            let _init_guard = init_span.enter();
            let result = catch_unwind(AssertUnwindSafe(|| entry.actor.init(&mut ctx)));
            let effects = ctx.finish();
            (result, effects)
        };

        match init_result {
            Ok(Ok(())) => {
                entry.state.initialized = true;

                if let Some(reason) = effects.exit_reason {
                    self.finish_actor(entry, reason);
                    return;
                }

                self.return_actor(entry);
            }
            Ok(Err(reason)) => self.finish_actor(entry, reason),
            Err(panic) => {
                let actor_id = entry.state.id;
                let name = entry.name;
                self.finish_actor(entry, panic_reason(actor_id, name, panic));
            }
        }
    }

    pub(super) fn return_actor(&mut self, mut entry: ActorEntry) {
        entry.state.metrics.mailbox_len = entry.state.mailbox.len();
        entry.state.metrics.scheduler_id = None;

        if entry.state.mailbox.is_empty() {
            park_actor(&mut entry.state);
        } else {
            entry.state.status = ActorStatus::Runnable;
            schedule_actor(&mut self.run_queue, &mut entry.state);
        }

        self.actors.insert(entry.state.id.local_id, entry);
    }

    pub(super) fn finish_actor(&mut self, mut entry: ActorEntry, reason: ExitReason) {
        entry.state.status = ActorStatus::Exiting;
        entry.state.metrics.scheduler_id = Some(0);

        let terminate_result = {
            let actor_id = entry.state.id;
            let actor_name = entry.name;
            let mut ctx = LocalContext::new(self, &mut entry.state, entry.name);
            let terminate_span = tracing::debug_span!(
                "lamport.actor.terminate",
                actor_id = %actor_id,
                actor_name = actor_name,
                scheduler_id = 0usize,
                reason = %reason
            );
            let _terminate_guard = terminate_span.enter();
            let result = catch_unwind(AssertUnwindSafe(|| {
                entry.actor.terminate(reason.clone(), &mut ctx);
            }));
            let _ = ctx.finish();
            result
        };

        let final_reason = match terminate_result {
            Ok(()) => reason,
            Err(panic) => panic_reason(entry.state.id, entry.name, panic),
        };

        self.registry.unregister(entry.state.id);

        self.cancel_all_timers(entry.state.id);
        self.cancel_all_call_timeouts(entry.state.id);

        let actor_id = entry.state.id;
        let parent = entry.state.parent;
        let ancestors = entry.state.ancestors.clone();
        let registered_name = entry.state.registered_name.clone();
        let supervisor_child = entry.state.supervisor_child;
        let linked: Vec<_> = entry.state.links.iter().copied().collect();
        let monitored_by: Vec<_> = entry
            .state
            .monitors_in
            .iter()
            .map(|(reference, watcher)| (*reference, *watcher))
            .collect();
        let monitoring: Vec<_> = entry
            .state
            .monitors_out
            .iter()
            .map(|(reference, target)| (*reference, *target))
            .collect();
        let linked_reason =
            shutdown_link_reason(self.shutdown_tasks.contains_key(&actor_id), &final_reason);

        if let (Some(parent), Some(child_id)) = (parent, supervisor_child)
            && let Some(parent_state) = self.actor_state_mut(parent)
            && let Some(supervisor) = parent_state.supervisor.as_mut()
        {
            supervisor.clear_running(child_id, actor_id);
        }

        self.remove_outgoing_monitors(actor_id, monitoring);
        self.notify_incoming_monitors(actor_id, &final_reason, monitored_by);
        self.detach_links_and_propagate(actor_id, &linked_reason, linked);

        if let Some(mut tracker) = self.shutdown_tasks.remove(&actor_id) {
            if let Some(task) = tracker.task.take() {
                task.abort();
            }

            self.record_lifecycle_event(LifecycleEvent::Shutdown {
                requester: tracker.requester,
                actor: actor_id,
                policy: tracker.policy,
                phase: ShutdownPhase::Completed,
                reason: Some(final_reason.clone()),
            });
        }

        entry.state.links.clear();
        entry.state.monitors_in.clear();
        entry.state.monitors_out.clear();
        entry.state.suspended = false;
        entry.state.status = ActorStatus::Dead;
        entry.state.metrics.last_exit = Some(final_reason.clone());
        entry.state.metrics.mailbox_len = 0;
        entry.state.metrics.scheduler_id = None;
        entry.state.scheduled = false;
        entry.state.registered_name = None;

        self.record_lifecycle_event(LifecycleEvent::Exit {
            actor: actor_id,
            name: entry.name,
            reason: final_reason.clone(),
            parent,
            ancestors: ancestors.clone(),
        });

        if !matches!(final_reason, ExitReason::Normal | ExitReason::Shutdown) {
            let parent_context = parent.and_then(|parent| self.actor_identity(parent));
            let ancestor_contexts = ancestors
                .iter()
                .filter_map(|ancestor| self.actor_identity(*ancestor))
                .collect();
            self.record_crash_report(CrashReport {
                actor: actor_id,
                name: entry.name,
                registered_name,
                parent,
                parent_context,
                ancestors: ancestors.clone(),
                ancestor_contexts,
                supervisor_child,
                reason: final_reason.clone(),
            });
        }

        let id = entry.state.id;
        self.completed.insert(id, entry.snapshot());
        self.actor_ids.release(id);
        self.live_actors = self.live_actors.saturating_sub(1);
    }

    pub(super) fn remove_outgoing_monitors(
        &mut self,
        actor: ActorId,
        monitoring: Vec<(Ref, ActorId)>,
    ) {
        for (reference, target) in monitoring {
            if target == actor {
                continue;
            }

            if let Some(target_state) = self.actor_state_mut(target) {
                target_state.monitors_in.remove(&reference);
            }
        }
    }

    pub(super) fn notify_incoming_monitors(
        &mut self,
        actor: ActorId,
        reason: &ExitReason,
        monitored_by: Vec<(Ref, ActorId)>,
    ) {
        for (reference, watcher) in monitored_by {
            if watcher == actor {
                continue;
            }

            let scheduled = if let Some(watcher_state) = self.actor_state_mut(watcher) {
                watcher_state.monitors_out.remove(&reference);
                push_envelope(
                    watcher_state,
                    Envelope::Down(DownMessage {
                        reference,
                        actor: actor.into(),
                        reason: reason.clone(),
                    }),
                )
            } else {
                Ok(None)
            };

            match scheduled {
                Ok(Some(actor_id)) => self.run_queue.push_back(actor_id),
                Ok(None) => {}
                Err(SendError::MailboxFull { .. }) => {
                    self.force_exit(watcher, mailbox_overflow_reason(watcher, "down"));
                }
                Err(SendError::NoProc(_)) => {}
            }

            self.record_lifecycle_event(LifecycleEvent::Down {
                watcher,
                actor,
                reference,
                reason: reason.clone(),
            });
        }
    }

    pub(super) fn detach_links_and_propagate(
        &mut self,
        actor: ActorId,
        reason: &ExitReason,
        linked: Vec<ActorId>,
    ) {
        for linked_actor in linked {
            if linked_actor == actor {
                continue;
            }

            if let Some(other) = self.actor_state_mut(linked_actor) {
                other.links.remove(&actor);
            }

            self.propagate_link_exit(
                linked_actor,
                ExitSignal {
                    from: actor.into(),
                    reason: reason.clone(),
                    linked: true,
                },
            );
        }
    }

    pub(super) fn propagate_link_exit(&mut self, target: ActorId, signal: ExitSignal) {
        let Some(trap_exit) = self
            .actor_state_mut(target)
            .map(|target_state| target_state.trap_exit)
        else {
            return;
        };

        match classify_exit_signal(trap_exit, &signal) {
            ExitDisposition::KillNow => {
                self.force_exit(target, signal.reason);
            }
            ExitDisposition::Ignore => {}
            ExitDisposition::Enqueue => {
                let _ = self.enqueue_runtime_envelope(target, Envelope::Exit(signal), "exit");
            }
        }
    }

    pub(super) fn force_exit(&mut self, actor: ActorId, reason: ExitReason) -> bool {
        let Some(entry) = self.take_actor(actor) else {
            return false;
        };

        self.finish_actor(entry, reason);
        true
    }
}
