use std::time::Duration;

use crate::{
    actor::Actor,
    context::{
        ActorContext, Context, LifecycleContext, LinkError, MonitorError, PendingCall, SendError,
        SpawnError, SpawnOptions, SupervisorContext, TaskHandle, TimerError,
    },
    envelope::{Envelope, Message},
    lifecycle::LifecycleEvent,
    registry::RegistryError,
    types::{ActorId, ChildSpec, ExitReason, Ref, Shutdown, SupervisorFlags, TimerToken},
};

pub(super) struct SupervisorChildContext<'a, C> {
    pub(super) inner: &'a mut C,
    pub(super) child_id: &'static str,
    pub(super) spawned_actor: Option<ActorId>,
}

impl<'a, C> SupervisorChildContext<'a, C> {
    pub(super) fn new(inner: &'a mut C, child_id: &'static str) -> Self {
        Self {
            inner,
            child_id,
            spawned_actor: None,
        }
    }
}

impl<C: Context> ActorContext for SupervisorChildContext<'_, C> {
    fn actor_id(&self) -> ActorId {
        self.inner.actor_id()
    }

    fn scheduler_id(&self) -> Option<usize> {
        self.inner.scheduler_id()
    }

    fn spawn<A: Actor>(
        &mut self,
        actor: A,
        mut options: SpawnOptions,
    ) -> Result<ActorId, SpawnError> {
        options.link_to_parent = true;
        options.supervisor_child = Some(self.child_id);
        let actor_id = self.inner.spawn(actor, options)?;
        self.spawned_actor = Some(actor_id);
        Ok(actor_id)
    }

    fn whereis(&self, name: &str) -> Option<ActorId> {
        self.inner.whereis(name)
    }

    fn register_name(&mut self, name: String) -> Result<(), RegistryError> {
        self.inner.register_name(name)
    }

    fn unregister_name(&mut self) -> Option<String> {
        self.inner.unregister_name()
    }

    fn send_envelope(&mut self, to: ActorId, envelope: Envelope) -> Result<(), SendError> {
        self.inner.send_envelope(to, envelope)
    }

    fn ask<M: Message>(
        &mut self,
        to: ActorId,
        message: M,
        timeout: Option<Duration>,
    ) -> Result<PendingCall, SendError> {
        self.inner.ask(to, message, timeout)
    }

    fn link(&mut self, other: ActorId) -> Result<(), LinkError> {
        self.inner.link(other)
    }

    fn unlink(&mut self, other: ActorId) -> Result<(), LinkError> {
        self.inner.unlink(other)
    }

    fn monitor(&mut self, other: ActorId) -> Result<Ref, MonitorError> {
        self.inner.monitor(other)
    }

    fn demonitor(&mut self, reference: Ref) -> Result<(), MonitorError> {
        self.inner.demonitor(reference)
    }

    fn set_trap_exit(&mut self, enabled: bool) {
        self.inner.set_trap_exit(enabled);
    }

    fn schedule_after(&mut self, delay: Duration, token: TimerToken) -> Result<(), TimerError> {
        self.inner.schedule_after(delay, token)
    }

    fn cancel_timer(&mut self, token: TimerToken) -> bool {
        self.inner.cancel_timer(token)
    }

    fn spawn_blocking_io<F, R>(&mut self, job: F) -> TaskHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        self.inner.spawn_blocking_io(job)
    }

    fn spawn_blocking_cpu<F, R>(&mut self, job: F) -> TaskHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        self.inner.spawn_blocking_cpu(job)
    }

    fn yield_now(&mut self) {
        self.inner.yield_now();
    }

    fn exit(&mut self, reason: ExitReason) {
        self.inner.exit(reason);
    }

    fn shutdown_actor(&mut self, actor: ActorId, policy: Shutdown) -> Result<(), SendError> {
        self.inner.shutdown_actor(actor, policy)
    }
}

impl<C: Context> SupervisorContext for SupervisorChildContext<'_, C> {
    fn configure_supervisor(&mut self, flags: SupervisorFlags, child_specs: Vec<ChildSpec>) {
        self.inner.configure_supervisor(flags, child_specs);
    }

    fn record_supervisor_restart(&mut self) -> bool {
        self.inner.record_supervisor_restart()
    }

    fn supervisor_child_started(&mut self, child_id: &'static str, actor: ActorId) {
        self.inner.supervisor_child_started(child_id, actor);
    }

    fn supervisor_child_exited(&mut self, child_id: &'static str, actor: ActorId) {
        self.inner.supervisor_child_exited(child_id, actor);
    }
}

impl<C: Context> LifecycleContext for SupervisorChildContext<'_, C> {
    fn emit_lifecycle_event(&mut self, event: LifecycleEvent) {
        self.inner.emit_lifecycle_event(event);
    }
}
