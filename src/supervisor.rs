use std::{
    collections::{BTreeMap, VecDeque},
    time::{Duration, Instant},
};

use crate::{
    actor::{Actor, ActorTurn},
    context::{
        Context, LinkError, MonitorError, PendingCall, SendError, SpawnError, SpawnOptions,
        TaskHandle, TimerError,
    },
    envelope::{Envelope, ExitSignal},
    registry::RegistryError,
    types::{
        ActorId, ChildSpec, ExitReason, LifecycleEvent, Ref, Shutdown, Strategy, SupervisorFlags,
        TimerToken,
    },
};

/// Failure to start a child from a supervisor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartChildError {
    /// The runtime rejected the spawn request.
    SpawnRejected,
    /// The child is already running.
    AlreadyStarted(&'static str),
    /// The child failed during initialization.
    InitFailed(ExitReason),
}

/// Action a supervisor should take after observing a child exit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisorDirective {
    /// No restart is needed.
    Ignore,
    /// Restart the listed child ids in order.
    Restart(Vec<&'static str>),
    /// Terminate the subtree.
    Shutdown(ExitReason),
}

/// Ordinary-actor API for supervisor processes.
pub trait Supervisor: Send + 'static {
    /// Returns the restart strategy and intensity window.
    fn flags(&self) -> SupervisorFlags;

    /// Returns the declared children in start order.
    fn child_specs(&self) -> &[ChildSpec];

    /// Starts one child from its spec and returns the running actor id.
    fn start_child<C: Context>(
        &mut self,
        spec: &ChildSpec,
        ctx: &mut C,
    ) -> Result<ActorId, StartChildError>;

    /// Handles a child exit and decides whether to restart or shut down.
    fn on_child_exit<C: Context>(
        &mut self,
        spec: &ChildSpec,
        actor: ActorId,
        reason: ExitReason,
        ctx: &mut C,
    ) -> SupervisorDirective;
}

/// Actor adapter that executes a typed supervisor on the runtime.
pub struct SupervisorActor<S: Supervisor> {
    supervisor: S,
    state: SupervisorActorState,
}

#[derive(Debug)]
struct SupervisorActorState {
    flags: SupervisorFlags,
    specs: Vec<ChildSpec>,
    running: BTreeMap<&'static str, ActorId>,
    by_actor: BTreeMap<ActorId, &'static str>,
    action: Option<SupervisorAction>,
}

#[derive(Debug)]
enum SupervisorAction {
    Restart(RestartPlan),
    Shutdown(ShutdownPlan),
}

#[derive(Debug)]
struct RestartPlan {
    active_shutdown: Option<&'static str>,
    shutdown_queue: VecDeque<&'static str>,
    restart_queue: VecDeque<&'static str>,
    old_children: BTreeMap<&'static str, Option<ActorId>>,
}

#[derive(Debug)]
struct ShutdownPlan {
    active_child: Option<&'static str>,
    shutdown_queue: VecDeque<&'static str>,
    final_reason: ExitReason,
}

struct SupervisorChildContext<'a, C> {
    inner: &'a mut C,
    child_id: &'static str,
    spawned_actor: Option<ActorId>,
}

impl<S: Supervisor> SupervisorActor<S> {
    /// Wraps a typed supervisor as a low-level actor.
    pub fn new(supervisor: S) -> Self {
        let flags = supervisor.flags();
        let specs = supervisor.child_specs().to_vec();

        Self {
            supervisor,
            state: SupervisorActorState::new(flags, specs),
        }
    }
}

impl SupervisorActorState {
    fn new(flags: SupervisorFlags, specs: Vec<ChildSpec>) -> Self {
        Self {
            flags,
            specs,
            running: BTreeMap::new(),
            by_actor: BTreeMap::new(),
            action: None,
        }
    }

    fn spec(&self, child_id: &'static str) -> Option<&ChildSpec> {
        self.specs.iter().find(|spec| spec.id == child_id)
    }

    fn running_actor(&self, child_id: &'static str) -> Option<ActorId> {
        self.running.get(child_id).copied()
    }

    fn remember_child(&mut self, child_id: &'static str, actor: ActorId) {
        self.running.insert(child_id, actor);
        self.by_actor.insert(actor, child_id);
    }

    fn forget_child_actor(&mut self, actor: ActorId) -> Option<&'static str> {
        let child_id = self.by_actor.remove(&actor)?;
        self.running.remove(child_id);
        Some(child_id)
    }

    fn ordered_child_ids(&self, requested: Vec<&'static str>) -> Vec<&'static str> {
        self.specs
            .iter()
            .filter_map(|spec| requested.contains(&spec.id).then_some(spec.id))
            .collect()
    }

    fn running_children_reverse(&self, child_ids: &[&'static str]) -> VecDeque<&'static str> {
        self.specs
            .iter()
            .rev()
            .filter_map(|spec| {
                (child_ids.contains(&spec.id) && self.running.contains_key(spec.id))
                    .then_some(spec.id)
            })
            .collect()
    }

    fn all_running_reverse(&self) -> VecDeque<&'static str> {
        self.specs
            .iter()
            .rev()
            .filter_map(|spec| self.running.contains_key(spec.id).then_some(spec.id))
            .collect()
    }
}

impl<'a, C> SupervisorChildContext<'a, C> {
    fn new(inner: &'a mut C, child_id: &'static str) -> Self {
        Self {
            inner,
            child_id,
            spawned_actor: None,
        }
    }
}

impl<C: Context> Context for SupervisorChildContext<'_, C> {
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

    fn ask<M: crate::envelope::Message>(
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

    fn shutdown_actor(&mut self, actor: ActorId, policy: Shutdown) -> Result<(), SendError> {
        self.inner.shutdown_actor(actor, policy)
    }

    fn emit_lifecycle_event(&mut self, event: LifecycleEvent) {
        self.inner.emit_lifecycle_event(event);
    }
}

impl<S: Supervisor> Actor for SupervisorActor<S> {
    fn name(&self) -> &'static str {
        std::any::type_name::<S>()
    }

    fn init<C: Context>(&mut self, ctx: &mut C) -> Result<(), ExitReason> {
        ctx.set_trap_exit(true);
        ctx.configure_supervisor(self.state.flags, self.state.specs.clone());

        for spec in self.state.specs.clone() {
            if let Err(error) = self.start_child(spec.id, ctx) {
                self.abort_startup(ctx);
                return Err(start_child_failure(&spec, error));
            }
        }

        Ok(())
    }

    fn handle<C: Context>(&mut self, envelope: Envelope, ctx: &mut C) -> ActorTurn {
        match envelope {
            Envelope::Exit(signal) => self.handle_exit(signal, ctx),
            Envelope::System(crate::envelope::SystemMessage::Shutdown) => {
                self.begin_shutdown(ExitReason::Shutdown, ctx)
            }
            _ => ActorTurn::Continue,
        }
    }
}

impl<S: Supervisor> SupervisorActor<S> {
    fn handle_exit<C: Context>(&mut self, signal: ExitSignal, ctx: &mut C) -> ActorTurn {
        if self.state.by_actor.contains_key(&signal.from) {
            return self.handle_child_exit(signal.from, signal.reason, ctx);
        }

        if matches!(self.state.action, Some(SupervisorAction::Shutdown(_))) {
            return ActorTurn::Continue;
        }

        self.state.action = None;
        self.begin_shutdown(signal.reason, ctx)
    }

    fn handle_child_exit<C: Context>(
        &mut self,
        actor: ActorId,
        reason: ExitReason,
        ctx: &mut C,
    ) -> ActorTurn {
        let Some(child_id) = self.state.forget_child_actor(actor) else {
            return ActorTurn::Continue;
        };
        ctx.supervisor_child_exited(child_id, actor);

        match self.state.action.take() {
            Some(SupervisorAction::Shutdown(mut plan)) => {
                self.advance_shutdown(&mut plan, child_id, ctx)
            }
            Some(SupervisorAction::Restart(mut plan)) => {
                self.advance_restart(&mut plan, child_id, ctx)
            }
            None => {
                let spec = self
                    .state
                    .spec(child_id)
                    .cloned()
                    .expect("child spec must exist for tracked child");

                match self.supervisor.on_child_exit(&spec, actor, reason, ctx) {
                    SupervisorDirective::Ignore => ActorTurn::Continue,
                    SupervisorDirective::Restart(children) => {
                        self.begin_restart(children, Some((child_id, actor)), ctx)
                    }
                    SupervisorDirective::Shutdown(reason) => self.begin_shutdown(reason, ctx),
                }
            }
        }
    }

    fn begin_restart<C: Context>(
        &mut self,
        requested_children: Vec<&'static str>,
        failed_child: Option<(&'static str, ActorId)>,
        ctx: &mut C,
    ) -> ActorTurn {
        let child_ids = self.state.ordered_child_ids(requested_children);
        if child_ids.is_empty() {
            return ActorTurn::Continue;
        }

        if !ctx.record_supervisor_restart() {
            return self.begin_shutdown(
                ExitReason::Error("supervisor restart intensity exceeded".into()),
                ctx,
            );
        }

        let mut shutdown_queue = self.state.running_children_reverse(&child_ids);
        let old_children = child_ids
            .iter()
            .copied()
            .map(|child_id| {
                (
                    child_id,
                    self.state.running_actor(child_id).or_else(|| {
                        failed_child
                            .and_then(|(failed_id, actor)| (failed_id == child_id).then_some(actor))
                    }),
                )
            })
            .collect();

        let active_shutdown = shutdown_queue.pop_front();
        let mut plan = RestartPlan {
            active_shutdown,
            shutdown_queue,
            restart_queue: child_ids.iter().copied().collect(),
            old_children,
        };

        if let Some(child_id) = plan.active_shutdown {
            return self.request_restart_shutdown(&mut plan, child_id, ctx);
        }

        self.finish_restart(plan, ctx)
    }

    fn advance_restart<C: Context>(
        &mut self,
        plan: &mut RestartPlan,
        child_id: &'static str,
        ctx: &mut C,
    ) -> ActorTurn {
        if plan.active_shutdown == Some(child_id) {
            plan.active_shutdown = None;
        } else if let Some(index) = plan
            .shutdown_queue
            .iter()
            .position(|queued| *queued == child_id)
        {
            plan.shutdown_queue.remove(index);
        } else {
            return self.begin_shutdown(
                ExitReason::Error(format!(
                    "child `{child_id}` exited while a restart was in progress"
                )),
                ctx,
            );
        }

        if let Some(next_child) = plan.shutdown_queue.pop_front() {
            plan.active_shutdown = Some(next_child);
            return self.request_restart_shutdown(plan, next_child, ctx);
        }

        let plan = RestartPlan {
            active_shutdown: None,
            shutdown_queue: std::mem::take(&mut plan.shutdown_queue),
            restart_queue: std::mem::take(&mut plan.restart_queue),
            old_children: std::mem::take(&mut plan.old_children),
        };

        self.finish_restart(plan, ctx)
    }

    fn request_restart_shutdown<C: Context>(
        &mut self,
        plan: &mut RestartPlan,
        child_id: &'static str,
        ctx: &mut C,
    ) -> ActorTurn {
        let Some(actor) = self.state.running_actor(child_id) else {
            plan.active_shutdown = None;
            return self.advance_restart(plan, child_id, ctx);
        };

        let spec = self
            .state
            .spec(child_id)
            .cloned()
            .expect("restart target must have a spec");

        if ctx.shutdown_actor(actor, spec.shutdown).is_err() {
            self.state.forget_child_actor(actor);
            plan.active_shutdown = None;
            return self.advance_restart(plan, child_id, ctx);
        }

        self.state.action = Some(SupervisorAction::Restart(RestartPlan {
            active_shutdown: plan.active_shutdown,
            shutdown_queue: plan.shutdown_queue.clone(),
            restart_queue: plan.restart_queue.clone(),
            old_children: plan.old_children.clone(),
        }));

        ActorTurn::Continue
    }

    fn finish_restart<C: Context>(&mut self, mut plan: RestartPlan, ctx: &mut C) -> ActorTurn {
        while let Some(child_id) = plan.restart_queue.pop_front() {
            match self.start_child(child_id, ctx) {
                Ok(new_actor) => ctx.emit_lifecycle_event(LifecycleEvent::Restart {
                    supervisor: ctx.actor_id(),
                    child_id,
                    old_actor: plan.old_children.get(child_id).copied().flatten(),
                    new_actor,
                }),
                Err(error) => {
                    return self.begin_shutdown(
                        ExitReason::Error(format!(
                            "failed to restart child `{child_id}`: {}",
                            format_start_child_error(&error)
                        )),
                        ctx,
                    );
                }
            }
        }

        ActorTurn::Continue
    }

    fn begin_shutdown<C: Context>(&mut self, reason: ExitReason, ctx: &mut C) -> ActorTurn {
        let mut shutdown_queue = self.state.all_running_reverse();
        let active_child = shutdown_queue.pop_front();
        let plan = ShutdownPlan {
            active_child,
            shutdown_queue,
            final_reason: reason,
        };

        if let Some(child_id) = plan.active_child {
            return self.request_shutdown(plan, child_id, ctx);
        }

        ActorTurn::Stop(plan.final_reason)
    }

    fn advance_shutdown<C: Context>(
        &mut self,
        plan: &mut ShutdownPlan,
        child_id: &'static str,
        ctx: &mut C,
    ) -> ActorTurn {
        if plan.active_child == Some(child_id) {
            plan.active_child = None;
        } else if let Some(index) = plan
            .shutdown_queue
            .iter()
            .position(|queued| *queued == child_id)
        {
            plan.shutdown_queue.remove(index);
        }

        if let Some(next_child) = plan.shutdown_queue.pop_front() {
            plan.active_child = Some(next_child);
            return self.request_shutdown(
                ShutdownPlan {
                    active_child: plan.active_child,
                    shutdown_queue: plan.shutdown_queue.clone(),
                    final_reason: plan.final_reason.clone(),
                },
                next_child,
                ctx,
            );
        }

        ActorTurn::Stop(plan.final_reason.clone())
    }

    fn request_shutdown<C: Context>(
        &mut self,
        plan: ShutdownPlan,
        child_id: &'static str,
        ctx: &mut C,
    ) -> ActorTurn {
        let Some(actor) = self.state.running_actor(child_id) else {
            let mut plan = plan;
            plan.active_child = None;
            return self.advance_shutdown(&mut plan, child_id, ctx);
        };

        let spec = self
            .state
            .spec(child_id)
            .cloned()
            .expect("shutdown target must have a spec");

        if ctx.shutdown_actor(actor, spec.shutdown).is_err() {
            self.state.forget_child_actor(actor);
            let mut plan = plan;
            plan.active_child = None;
            return self.advance_shutdown(&mut plan, child_id, ctx);
        }

        self.state.action = Some(SupervisorAction::Shutdown(plan));
        ActorTurn::Continue
    }

    fn start_child<C: Context>(
        &mut self,
        child_id: &'static str,
        ctx: &mut C,
    ) -> Result<ActorId, StartChildError> {
        if self.state.running.contains_key(child_id) {
            return Err(StartChildError::AlreadyStarted(child_id));
        }

        let spec = self.state.spec(child_id).cloned().ok_or_else(|| {
            StartChildError::InitFailed(ExitReason::Error(format!(
                "unknown child spec `{child_id}`"
            )))
        })?;

        let mut child_ctx = SupervisorChildContext::new(ctx, child_id);
        let actor = self.supervisor.start_child(&spec, &mut child_ctx)?;

        if child_ctx.spawned_actor != Some(actor) {
            return Err(StartChildError::SpawnRejected);
        }

        self.state.remember_child(child_id, actor);
        ctx.supervisor_child_started(child_id, actor);
        Ok(actor)
    }

    fn abort_startup<C: Context>(&mut self, ctx: &mut C) {
        let running: Vec<_> = self.state.all_running_reverse().into_iter().collect();
        for child_id in running {
            if let Some(actor) = self.state.running_actor(child_id) {
                let _ = ctx.shutdown_actor(actor, Shutdown::BrutalKill);
            }
        }
    }
}

fn start_child_failure(spec: &ChildSpec, error: StartChildError) -> ExitReason {
    match error {
        StartChildError::SpawnRejected => {
            ExitReason::Error(format!("supervisor rejected child spawn for `{}`", spec.id))
        }
        StartChildError::AlreadyStarted(child_id) => {
            ExitReason::Error(format!("supervisor child `{child_id}` is already running"))
        }
        StartChildError::InitFailed(reason) => reason,
    }
}

fn format_start_child_error(error: &StartChildError) -> String {
    match error {
        StartChildError::SpawnRejected => "spawn rejected".into(),
        StartChildError::AlreadyStarted(child_id) => format!("child `{child_id}` already started"),
        StartChildError::InitFailed(reason) => format!("init failed: {reason:?}"),
    }
}

/// Computes which children should restart for a failed child under the strategy.
pub fn restart_scope(
    strategy: Strategy,
    specs: &[ChildSpec],
    failed_child: &'static str,
) -> Vec<&'static str> {
    match strategy {
        Strategy::OneForOne => specs
            .iter()
            .find(|spec| spec.id == failed_child)
            .map(|spec| vec![spec.id])
            .unwrap_or_default(),
        Strategy::OneForAll => specs.iter().map(|spec| spec.id).collect(),
        Strategy::RestForOne => {
            let start = specs.iter().position(|spec| spec.id == failed_child);

            start
                .map(|index| specs[index..].iter().map(|spec| spec.id).collect())
                .unwrap_or_default()
        }
    }
}

/// Sliding window used to enforce supervisor restart intensity.
#[derive(Debug, Clone)]
pub struct RestartIntensity {
    flags: SupervisorFlags,
    restarts: VecDeque<Instant>,
}

impl RestartIntensity {
    /// Creates a new restart-intensity tracker.
    pub fn new(flags: SupervisorFlags) -> Self {
        Self {
            flags,
            restarts: VecDeque::new(),
        }
    }

    /// Returns the supervisor flags for this tracker.
    pub fn flags(&self) -> &SupervisorFlags {
        &self.flags
    }

    /// Records a restart and returns `true` when the restart is still within limits.
    pub fn record_restart(&mut self, now: Instant) -> bool {
        self.prune(now);
        self.restarts.push_back(now);
        (self.restarts.len() as u32) <= self.flags.intensity
    }

    /// Returns the current restart count inside the active period.
    pub fn active_restarts(&mut self, now: Instant) -> usize {
        self.prune(now);
        self.restarts.len()
    }

    fn prune(&mut self, now: Instant) {
        let period = self.flags.period;

        while let Some(oldest) = self.restarts.front().copied() {
            if within_period(oldest, now, period) {
                break;
            }

            self.restarts.pop_front();
        }
    }
}

fn within_period(start: Instant, now: Instant, period: Duration) -> bool {
    now.checked_duration_since(start)
        .is_some_and(|elapsed| elapsed <= period)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };

    use crate::{
        Actor, ActorTurn, Context, DownMessage, Envelope, ExitReason, LifecycleEvent, LocalRuntime,
        SpawnOptions, SupervisorActor, TimerToken,
        envelope::{SystemMessage, TimerFired},
        types::{ActorStatus, ChildSpec, Restart, Shutdown, Strategy, SupervisorFlags},
    };

    use super::{
        RestartIntensity, StartChildError, Supervisor, SupervisorDirective, restart_scope,
    };

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Control {
        Crash,
    }

    #[derive(Debug, Clone)]
    enum WorkerMode {
        Immediate,
        DelayShutdown(Duration),
        IgnoreShutdown,
    }

    struct WorkerActor {
        id: &'static str,
        mode: WorkerMode,
        log: Arc<Mutex<Vec<String>>>,
        shutdown_timer: Option<TimerToken>,
    }

    impl Actor for WorkerActor {
        fn init<C: Context>(&mut self, ctx: &mut C) -> Result<(), ExitReason> {
            self.log
                .lock()
                .unwrap()
                .push(format!("start:{}:{}", self.id, ctx.actor_id().local_id));
            Ok(())
        }

        fn handle<C: Context>(&mut self, envelope: Envelope, ctx: &mut C) -> ActorTurn {
            match envelope {
                Envelope::User(payload)
                    if payload.downcast_ref::<Control>() == Some(&Control::Crash) =>
                {
                    ActorTurn::Stop(ExitReason::Error(format!("{} boom", self.id)))
                }
                Envelope::Exit(signal) if matches!(signal.reason, ExitReason::Shutdown) => {
                    match self.mode {
                        WorkerMode::Immediate => ActorTurn::Stop(ExitReason::Shutdown),
                        WorkerMode::DelayShutdown(delay) => {
                            let token = TimerToken::next();
                            self.shutdown_timer = Some(token);
                            ctx.schedule_after(delay, token)
                                .expect("shutdown timer should schedule");
                            ActorTurn::Continue
                        }
                        WorkerMode::IgnoreShutdown => ActorTurn::Continue,
                    }
                }
                Envelope::Timer(TimerFired { token }) if self.shutdown_timer == Some(token) => {
                    self.shutdown_timer = None;
                    ActorTurn::Stop(ExitReason::Shutdown)
                }
                _ => ActorTurn::Continue,
            }
        }

        fn terminate<C: Context>(&mut self, reason: ExitReason, _ctx: &mut C) {
            self.log
                .lock()
                .unwrap()
                .push(format!("stop:{}:{reason:?}", self.id));
        }
    }

    struct MonitorActor {
        target: crate::ActorId,
        seen: Arc<Mutex<Vec<DownMessage>>>,
    }

    impl Actor for MonitorActor {
        fn init<C: Context>(&mut self, ctx: &mut C) -> Result<(), ExitReason> {
            ctx.monitor(self.target)
                .map_err(|error| ExitReason::Error(format!("monitor failed: {error:?}")))?;
            Ok(())
        }

        fn handle<C: Context>(&mut self, envelope: Envelope, _ctx: &mut C) -> ActorTurn {
            if let Envelope::Down(message) = envelope {
                self.seen.lock().unwrap().push(message);
            }

            ActorTurn::Continue
        }
    }

    struct TestSupervisor {
        flags: SupervisorFlags,
        specs: Vec<ChildSpec>,
        modes: BTreeMap<&'static str, WorkerMode>,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl Supervisor for TestSupervisor {
        fn flags(&self) -> SupervisorFlags {
            self.flags
        }

        fn child_specs(&self) -> &[ChildSpec] {
            &self.specs
        }

        fn start_child<C: Context>(
            &mut self,
            spec: &ChildSpec,
            ctx: &mut C,
        ) -> Result<crate::ActorId, StartChildError> {
            let mode = self
                .modes
                .get(spec.id)
                .cloned()
                .expect("test mode must exist for each child");

            ctx.spawn(
                WorkerActor {
                    id: spec.id,
                    mode: mode.clone(),
                    log: Arc::clone(&self.log),
                    shutdown_timer: None,
                },
                SpawnOptions {
                    trap_exit: !matches!(mode, WorkerMode::Immediate),
                    ..SpawnOptions::default()
                },
            )
            .map_err(|_| StartChildError::SpawnRejected)
        }

        fn on_child_exit<C: Context>(
            &mut self,
            spec: &ChildSpec,
            _actor: crate::ActorId,
            reason: ExitReason,
            _ctx: &mut C,
        ) -> SupervisorDirective {
            if !spec.should_restart(&reason) {
                return SupervisorDirective::Ignore;
            }

            SupervisorDirective::Restart(restart_scope(self.flags.strategy, &self.specs, spec.id))
        }
    }

    fn test_supervisor(
        strategy: Strategy,
        intensity: u32,
        specs: Vec<ChildSpec>,
        modes: BTreeMap<&'static str, WorkerMode>,
        log: Arc<Mutex<Vec<String>>>,
    ) -> TestSupervisor {
        TestSupervisor {
            flags: SupervisorFlags {
                strategy,
                intensity,
                period: Duration::from_secs(5),
            },
            specs,
            modes,
            log,
        }
    }

    fn default_specs() -> Vec<ChildSpec> {
        ["a", "b", "c"]
            .into_iter()
            .map(|id| ChildSpec {
                id,
                restart: Restart::Permanent,
                shutdown: Shutdown::default(),
                is_supervisor: false,
            })
            .collect()
    }

    fn child_ids(
        runtime: &mut LocalRuntime,
        supervisor: crate::ActorId,
    ) -> BTreeMap<&'static str, crate::ActorId> {
        let snapshot = runtime
            .supervisor_snapshot(supervisor)
            .expect("supervisor snapshot should exist");
        let snapshot_debug = format!("{snapshot:?}");

        snapshot
            .children
            .into_iter()
            .map(|child| {
                let actor = child.actor.unwrap_or_else(|| {
                    panic!(
                        "child `{}` should be running: {snapshot_debug}",
                        child.spec.id
                    )
                });
                (child.spec.id, actor)
            })
            .collect()
    }

    fn wait_until_dead(runtime: &mut LocalRuntime, actor: crate::ActorId) {
        while runtime
            .actor_snapshot(actor)
            .is_some_and(|snapshot| snapshot.status != ActorStatus::Dead)
        {
            let progressed = runtime.block_on_next(Some(Duration::from_secs(1)));
            if !progressed {
                let snapshot = runtime.actor_snapshot(actor);
                let events = runtime.lifecycle_events().to_vec();
                panic!("actor did not make progress: snapshot={snapshot:?} events={events:?}");
            }
            runtime.run_until_idle();
        }
    }

    #[test]
    fn restart_scope_matches_otp_strategies() {
        let specs = vec![
            ChildSpec {
                id: "a",
                restart: Restart::Permanent,
                shutdown: Shutdown::default(),
                is_supervisor: false,
            },
            ChildSpec {
                id: "b",
                restart: Restart::Permanent,
                shutdown: Shutdown::default(),
                is_supervisor: false,
            },
            ChildSpec {
                id: "c",
                restart: Restart::Permanent,
                shutdown: Shutdown::default(),
                is_supervisor: false,
            },
        ];

        assert_eq!(restart_scope(Strategy::OneForOne, &specs, "b"), vec!["b"]);
        assert_eq!(
            restart_scope(Strategy::OneForAll, &specs, "b"),
            vec!["a", "b", "c"]
        );
        assert_eq!(
            restart_scope(Strategy::RestForOne, &specs, "b"),
            vec!["b", "c"]
        );
    }

    #[test]
    fn restart_intensity_prunes_old_entries() {
        let mut intensity = RestartIntensity::new(SupervisorFlags {
            strategy: Strategy::OneForOne,
            intensity: 2,
            period: Duration::from_secs(5),
        });

        let now = Instant::now();

        assert!(intensity.record_restart(now));
        assert!(intensity.record_restart(now + Duration::from_secs(1)));
        assert!(!intensity.record_restart(now + Duration::from_secs(2)));

        assert_eq!(intensity.active_restarts(now + Duration::from_secs(6)), 2);
        assert!(!intensity.record_restart(now + Duration::from_secs(6)));
        assert_eq!(intensity.active_restarts(now + Duration::from_secs(8)), 1);
        assert!(intensity.record_restart(now + Duration::from_secs(8)));
    }

    #[test]
    fn one_for_one_restart_updates_runtime_state_and_events() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let modes = BTreeMap::from([
            ("a", WorkerMode::Immediate),
            ("b", WorkerMode::Immediate),
            ("c", WorkerMode::Immediate),
        ]);
        let mut runtime = LocalRuntime::default();
        let supervisor = runtime
            .spawn(SupervisorActor::new(test_supervisor(
                Strategy::OneForOne,
                3,
                default_specs(),
                modes,
                Arc::clone(&log),
            )))
            .unwrap();

        runtime.run_until_idle();
        let before = child_ids(&mut runtime, supervisor);
        let down_seen = Arc::new(Mutex::new(Vec::new()));
        let watcher = runtime
            .spawn(MonitorActor {
                target: before["b"],
                seen: Arc::clone(&down_seen),
            })
            .unwrap();

        runtime.run_until_idle();
        runtime.send(before["b"], Control::Crash).unwrap();
        runtime.run_until_idle();

        let after = child_ids(&mut runtime, supervisor);
        assert_eq!(after["a"], before["a"]);
        assert_ne!(after["b"], before["b"]);
        assert_eq!(after["c"], before["c"]);
        assert_eq!(
            runtime
                .supervisor_snapshot(supervisor)
                .unwrap()
                .active_restarts,
            1
        );
        assert_eq!(down_seen.lock().unwrap().len(), 1);

        assert!(runtime.lifecycle_events().iter().any(|event| {
            matches!(
                event,
                LifecycleEvent::Restart {
                    supervisor: source,
                    child_id: "b",
                    old_actor: Some(old_actor),
                    new_actor,
                } if *source == supervisor && *old_actor == before["b"] && *new_actor == after["b"]
            )
        }));
        assert!(runtime.lifecycle_events().iter().any(|event| {
            matches!(
                event,
                LifecycleEvent::Down {
                    watcher: seen_by,
                    actor,
                    ..
                } if *seen_by == watcher && *actor == before["b"]
            )
        }));
        assert!(runtime.crash_reports().iter().any(|report| {
            report.actor == before["b"]
                && report.reason == ExitReason::Error("b boom".into())
                && report.supervisor_child == Some("b")
        }));
    }

    #[test]
    fn one_for_all_restarts_entire_subtree() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let modes = BTreeMap::from([
            ("a", WorkerMode::Immediate),
            ("b", WorkerMode::Immediate),
            ("c", WorkerMode::Immediate),
        ]);
        let mut runtime = LocalRuntime::default();
        let supervisor = runtime
            .spawn(SupervisorActor::new(test_supervisor(
                Strategy::OneForAll,
                3,
                default_specs(),
                modes,
                log,
            )))
            .unwrap();

        runtime.run_until_idle();
        let before = child_ids(&mut runtime, supervisor);
        runtime.send(before["b"], Control::Crash).unwrap();
        runtime.run_until_idle();

        let after = child_ids(&mut runtime, supervisor);
        assert_ne!(after["a"], before["a"]);
        assert_ne!(after["b"], before["b"]);
        assert_ne!(after["c"], before["c"]);

        let requested: Vec<_> = runtime
            .lifecycle_events()
            .iter()
            .filter_map(|event| match event {
                LifecycleEvent::Shutdown {
                    phase: crate::ShutdownPhase::Requested,
                    actor,
                    ..
                } => Some(*actor),
                _ => None,
            })
            .collect();

        assert_eq!(requested, vec![before["c"], before["a"]]);
    }

    #[test]
    fn rest_for_one_restarts_only_suffix() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let modes = BTreeMap::from([
            ("a", WorkerMode::Immediate),
            ("b", WorkerMode::Immediate),
            ("c", WorkerMode::Immediate),
        ]);
        let mut runtime = LocalRuntime::default();
        let supervisor = runtime
            .spawn(SupervisorActor::new(test_supervisor(
                Strategy::RestForOne,
                3,
                default_specs(),
                modes,
                log,
            )))
            .unwrap();

        runtime.run_until_idle();
        let before = child_ids(&mut runtime, supervisor);
        runtime.send(before["b"], Control::Crash).unwrap();
        runtime.run_until_idle();

        let after = child_ids(&mut runtime, supervisor);
        assert_eq!(after["a"], before["a"]);
        assert_ne!(after["b"], before["b"]);
        assert_ne!(after["c"], before["c"]);

        let requested: Vec<_> = runtime
            .lifecycle_events()
            .iter()
            .filter_map(|event| match event {
                LifecycleEvent::Shutdown {
                    phase: crate::ShutdownPhase::Requested,
                    actor,
                    ..
                } => Some(*actor),
                _ => None,
            })
            .collect();

        assert_eq!(requested, vec![before["c"]]);
    }

    #[test]
    fn shutdown_order_and_timeouts_are_enforced() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let specs = vec![
            ChildSpec {
                id: "a",
                restart: Restart::Permanent,
                shutdown: Shutdown::Timeout(Duration::from_millis(50)),
                is_supervisor: false,
            },
            ChildSpec {
                id: "b",
                restart: Restart::Permanent,
                shutdown: Shutdown::Timeout(Duration::from_millis(10)),
                is_supervisor: false,
            },
            ChildSpec {
                id: "c",
                restart: Restart::Permanent,
                shutdown: Shutdown::Timeout(Duration::from_millis(50)),
                is_supervisor: false,
            },
        ];
        let modes = BTreeMap::from([
            ("a", WorkerMode::Immediate),
            ("b", WorkerMode::IgnoreShutdown),
            ("c", WorkerMode::DelayShutdown(Duration::from_millis(5))),
        ]);
        let mut runtime = LocalRuntime::default();
        let supervisor = runtime
            .spawn(SupervisorActor::new(test_supervisor(
                Strategy::OneForOne,
                3,
                specs,
                modes,
                Arc::clone(&log),
            )))
            .unwrap();

        runtime.run_until_idle();
        let before = child_ids(&mut runtime, supervisor);
        runtime
            .send_envelope(supervisor, Envelope::System(SystemMessage::Shutdown))
            .unwrap();
        runtime.run_until_idle();
        wait_until_dead(&mut runtime, supervisor);

        let requested: Vec<_> = runtime
            .lifecycle_events()
            .iter()
            .filter_map(|event| match event {
                LifecycleEvent::Shutdown {
                    phase: crate::ShutdownPhase::Requested,
                    actor,
                    ..
                } if *actor != supervisor => Some(*actor),
                _ => None,
            })
            .collect();

        assert_eq!(requested, vec![before["c"], before["b"], before["a"]]);
        assert!(runtime.lifecycle_events().iter().any(|event| {
            matches!(
                event,
                LifecycleEvent::Shutdown {
                    actor,
                    phase: crate::ShutdownPhase::TimedOut,
                    reason: Some(ExitReason::Kill),
                    ..
                } if *actor == before["b"]
            )
        }));
        assert_eq!(
            runtime
                .actor_snapshot(supervisor)
                .unwrap()
                .metrics
                .last_exit,
            Some(ExitReason::Shutdown)
        );
        assert!(
            runtime
                .crash_reports()
                .iter()
                .any(|report| report.actor == before["b"] && report.reason == ExitReason::Kill)
        );
    }

    #[test]
    fn restart_intensity_limit_shuts_down_supervisor() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let modes = BTreeMap::from([
            ("a", WorkerMode::Immediate),
            ("b", WorkerMode::Immediate),
            ("c", WorkerMode::Immediate),
        ]);
        let mut runtime = LocalRuntime::default();
        let supervisor = runtime
            .spawn(SupervisorActor::new(test_supervisor(
                Strategy::OneForOne,
                1,
                default_specs(),
                modes,
                log,
            )))
            .unwrap();

        runtime.run_until_idle();
        let before = child_ids(&mut runtime, supervisor);
        runtime.send(before["b"], Control::Crash).unwrap();
        runtime.run_until_idle();

        let after_first = child_ids(&mut runtime, supervisor);
        runtime.send(after_first["b"], Control::Crash).unwrap();
        runtime.run_until_idle();
        wait_until_dead(&mut runtime, supervisor);

        let snapshot = runtime.actor_snapshot(supervisor).unwrap();
        assert_eq!(snapshot.status, ActorStatus::Dead);
        assert_eq!(
            snapshot.metrics.last_exit,
            Some(ExitReason::Error(
                "supervisor restart intensity exceeded".into()
            ))
        );
    }
}
