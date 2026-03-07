use crate::{
    envelope::{DownMessage, Envelope, ExitSignal},
    internal::{classify_exit_signal, shutdown_link_reason},
    lifecycle::{CrashReport, LifecycleEvent, ShutdownPhase},
    types::{ActorId, ActorStatus, ExitReason},
};

use super::RuntimeShared;

impl RuntimeShared {
    pub(super) fn propagate_link_exit(&self, target: ActorId, signal: ExitSignal) {
        let trap_exit = {
            let state = self.lock_state();
            let Some(target_state) = Self::actor_ref(&state, target) else {
                return;
            };

            target_state.trap_exit
        };

        match classify_exit_signal(trap_exit, &signal) {
            crate::internal::ExitDisposition::KillNow => {
                self.request_force_exit(target, signal.reason);
            }
            crate::internal::ExitDisposition::Ignore => {}
            crate::internal::ExitDisposition::Enqueue => {
                let _ = self.enqueue_runtime_envelope(target, Envelope::Exit(signal), "exit");
            }
        }
    }

    pub(super) fn finalize_actor(
        &self,
        actor: ActorId,
        name: &'static str,
        final_reason: ExitReason,
    ) {
        let (linked, monitored_by, monitoring, shutdown_requested) = {
            let mut state = self.lock_state();
            let Some(mut entry) = state.actors.remove(&actor.local_id) else {
                return;
            };
            if entry.id != actor {
                state.actors.insert(entry.id.local_id, entry);
                return;
            }

            state.registry.unregister(actor);

            for (_, handle) in entry.timer_tasks.drain() {
                handle.abort();
            }
            for (_, handle) in entry.call_timeout_tasks.drain() {
                handle.abort();
            }

            let parent = entry.parent;
            let ancestors = entry.ancestors.clone();
            let registered_name = entry.registered_name.clone();
            let supervisor_child = entry.supervisor_child;
            let linked: Vec<_> = entry.links.iter().copied().collect();
            let monitored_by: Vec<_> = entry
                .monitors_in
                .iter()
                .map(|(reference, watcher)| (*reference, *watcher))
                .collect();
            let monitoring: Vec<_> = entry
                .monitors_out
                .iter()
                .map(|(reference, target)| (*reference, *target))
                .collect();

            if let (Some(parent), Some(child_id)) = (parent, supervisor_child)
                && let Some(parent_state) = Self::actor_mut(&mut state, parent)
                && let Some(supervisor) = parent_state.supervisor.as_mut()
            {
                supervisor.clear_running(child_id, actor);
            }

            let mut shutdown_tracker = entry.shutdown.take();
            if let Some(tracker) = shutdown_tracker.as_mut() {
                if let Some(task) = tracker.task.take() {
                    task.abort();
                }

                RuntimeShared::record_lifecycle_event(
                    &mut state,
                    LifecycleEvent::Shutdown {
                        requester: tracker.requester,
                        actor,
                        policy: tracker.policy.clone(),
                        phase: ShutdownPhase::Completed,
                        reason: Some(final_reason.clone()),
                    },
                );
            }

            entry.status = ActorStatus::Dead;
            entry.metrics.last_exit = Some(final_reason.clone());
            entry.metrics.mailbox_len = 0;
            entry.metrics.scheduler_id = None;
            entry.registered_name = None;
            let snapshot = entry.snapshot();

            RuntimeShared::record_lifecycle_event(
                &mut state,
                LifecycleEvent::Exit {
                    actor,
                    name,
                    reason: final_reason.clone(),
                    parent,
                    ancestors: ancestors.clone(),
                },
            );

            if !matches!(final_reason, ExitReason::Normal | ExitReason::Shutdown) {
                let parent_context = parent
                    .and_then(|parent| RuntimeShared::actor_identity_from_state(&state, parent));
                let ancestor_contexts = ancestors
                    .iter()
                    .filter_map(|ancestor| {
                        RuntimeShared::actor_identity_from_state(&state, *ancestor)
                    })
                    .collect();

                RuntimeShared::record_crash_report(
                    &mut state,
                    CrashReport {
                        actor,
                        name,
                        registered_name: registered_name.clone(),
                        parent,
                        parent_context,
                        ancestors: ancestors.clone(),
                        ancestor_contexts,
                        supervisor_child,
                        reason: final_reason.clone(),
                    },
                );
            }

            state.completed.insert(actor, snapshot.clone());
            state.live_actors = state.live_actors.saturating_sub(1);
            (linked, monitored_by, monitoring, shutdown_tracker.is_some())
        };

        for (reference, target) in monitoring {
            if target == actor {
                continue;
            }

            let mut state = self.lock_state();
            if let Some(target_state) = Self::actor_mut(&mut state, target) {
                target_state.monitors_in.remove(&reference);
            }
        }

        for (reference, watcher) in monitored_by {
            if watcher == actor {
                continue;
            }

            {
                let mut state = self.lock_state();
                if let Some(watcher_state) = Self::actor_mut(&mut state, watcher) {
                    watcher_state.monitors_out.remove(&reference);
                }
                RuntimeShared::record_lifecycle_event(
                    &mut state,
                    LifecycleEvent::Down {
                        watcher,
                        actor,
                        reference,
                        reason: final_reason.clone(),
                    },
                );
            }

            let _ = self.enqueue_runtime_envelope(
                watcher,
                Envelope::Down(DownMessage {
                    reference,
                    actor,
                    reason: final_reason.clone(),
                }),
                "down",
            );
        }

        let linked_reason = shutdown_link_reason(shutdown_requested, &final_reason);

        for linked_actor in linked {
            if linked_actor == actor {
                continue;
            }

            {
                let mut state = self.lock_state();
                if let Some(other) = Self::actor_mut(&mut state, linked_actor) {
                    other.links.remove(&actor);
                }
            }

            self.propagate_link_exit(
                linked_actor,
                ExitSignal {
                    from: actor,
                    reason: linked_reason.clone(),
                    linked: true,
                },
            );
        }

        self.idle_cv.notify_all();
    }
}
