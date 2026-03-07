mod lifecycle;
mod worker;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

use tokio::{
    runtime::{Handle as TokioHandle, Runtime as TokioRuntime},
    sync::Notify,
    task::JoinHandle,
};

use crate::{
    actor::{Actor, ActorTurn},
    behaviour::{GenServer, GenServerActor, GenStatem, GenStatemActor, RuntimeInfo},
    context::{
        ActorContext, LifecycleContext, LinkError, MonitorError, PendingCall, SendError,
        SpawnError, SpawnOptions, SupervisorContext, TaskHandle, TimerError,
    },
    envelope::{
        DownMessage, Envelope, ExitSignal, Message, Payload, ReplyToken, TaskCompleted, TimerFired,
    },
    internal::{
        ShutdownTracker, SupervisorRuntimeState, TurnEffects, mailbox_overflow_reason,
        normalize_scheduler_config,
    },
    lifecycle::{CrashReport, LifecycleEvent, ShutdownPhase},
    mailbox::{Mailbox, MailboxFull},
    observability::{
        ActorTree, EventCursor, RuntimeEvent, RuntimeEventKind, RuntimeIntrospection,
        RuntimeMetricsSnapshot, build_actor_tree, build_metrics_snapshot,
        build_runtime_introspection, emit_tracing_event, events_since,
    },
    registry::{Registry, RegistryError},
    scheduler::{
        PoolKind, RunQueueSnapshot, ScheduleError, Scheduler, SchedulerConfig, SchedulerMetrics,
    },
    snapshot::{ActorSnapshot, SupervisorSnapshot},
    supervisor::{Supervisor, SupervisorActor},
    types::{
        ActorId, ActorIdentity, ActorMetrics, ActorStatus, ChildSpec, ExitReason, Ref, Shutdown,
        SupervisorFlags, TimerToken,
    },
};

trait ActorCell: Send {
    fn init(&mut self, ctx: &mut ConcurrentContext) -> Result<(), ExitReason>;
    fn handle(&mut self, envelope: Envelope, ctx: &mut ConcurrentContext) -> ActorTurn;
    fn terminate(&mut self, reason: ExitReason, ctx: &mut ConcurrentContext);
}

impl<A: Actor> ActorCell for A {
    fn init(&mut self, ctx: &mut ConcurrentContext) -> Result<(), ExitReason> {
        Actor::init(self, ctx)
    }

    fn handle(&mut self, envelope: Envelope, ctx: &mut ConcurrentContext) -> ActorTurn {
        Actor::handle(self, envelope, ctx)
    }

    fn terminate(&mut self, reason: ExitReason, ctx: &mut ConcurrentContext) {
        Actor::terminate(self, reason, ctx);
    }
}

#[derive(Debug)]
struct ActorRecord {
    id: ActorId,
    name: &'static str,
    notify: Arc<Notify>,
    mailbox: Mailbox,
    links: BTreeSet<ActorId>,
    monitors_in: BTreeMap<Ref, ActorId>,
    monitors_out: BTreeMap<Ref, ActorId>,
    trap_exit: bool,
    status: ActorStatus,
    metrics: ActorMetrics,
    parent: Option<ActorId>,
    ancestors: Vec<ActorId>,
    registered_name: Option<String>,
    supervisor_child: Option<&'static str>,
    supervisor: Option<SupervisorRuntimeState>,
    initialized: bool,
    forced_exit: Option<ExitReason>,
    shutdown: Option<ShutdownTracker>,
    timer_tasks: HashMap<TimerToken, JoinHandle<()>>,
    call_timeout_tasks: HashMap<Ref, JoinHandle<()>>,
    scheduler_id: usize,
}

impl ActorRecord {
    fn new(
        id: ActorId,
        name: &'static str,
        options: SpawnOptions,
        config: &SchedulerConfig,
        scheduler_id: usize,
    ) -> Self {
        let mailbox_capacity = options
            .mailbox_capacity
            .unwrap_or(config.default_mailbox_capacity);

        Self {
            id,
            name,
            notify: Arc::new(Notify::new()),
            mailbox: Mailbox::with_limits(
                mailbox_capacity,
                config
                    .mailbox_runtime_reserve
                    .min(mailbox_capacity.saturating_sub(1)),
            ),
            links: BTreeSet::new(),
            monitors_in: BTreeMap::new(),
            monitors_out: BTreeMap::new(),
            trap_exit: options.trap_exit,
            status: ActorStatus::Starting,
            metrics: ActorMetrics::default(),
            parent: options.parent,
            ancestors: options.ancestors,
            registered_name: options.registered_name,
            supervisor_child: options.supervisor_child,
            supervisor: None,
            initialized: false,
            forced_exit: None,
            shutdown: None,
            timer_tasks: HashMap::new(),
            call_timeout_tasks: HashMap::new(),
            scheduler_id,
        }
    }

    fn snapshot(&self) -> ActorSnapshot {
        ActorSnapshot {
            id: self.id,
            name: self.name,
            registered_name: self.registered_name.clone(),
            parent: self.parent,
            ancestors: self.ancestors.clone(),
            links: self.links.iter().copied().collect(),
            monitors_in: self
                .monitors_in
                .iter()
                .map(|(reference, actor)| (*reference, *actor))
                .collect(),
            monitors_out: self
                .monitors_out
                .iter()
                .map(|(reference, actor)| (*reference, *actor))
                .collect(),
            trap_exit: self.trap_exit,
            status: self.status,
            metrics: self.metrics.clone(),
        }
    }
}

#[derive(Debug, Default)]
struct RuntimeState {
    next_local_id: u64,
    live_actors: usize,
    actors: HashMap<u64, ActorRecord>,
    completed: HashMap<ActorId, ActorSnapshot>,
    registry: Registry,
    lifecycle_events: Vec<LifecycleEvent>,
    crash_reports: Vec<CrashReport>,
    event_log: Vec<RuntimeEvent>,
    next_event_sequence: u64,
    metrics: SchedulerMetrics,
}

struct RuntimeShared {
    config: SchedulerConfig,
    actor_handle: TokioHandle,
    cpu_handle: TokioHandle,
    state: Mutex<RuntimeState>,
    idle_cv: Condvar,
    started_at: Instant,
}

impl RuntimeShared {
    fn actor_ref(state: &RuntimeState, actor: ActorId) -> Option<&ActorRecord> {
        state
            .actors
            .get(&actor.local_id)
            .and_then(|entry| (entry.id == actor).then_some(entry))
    }

    fn actor_mut(state: &mut RuntimeState, actor: ActorId) -> Option<&mut ActorRecord> {
        state
            .actors
            .get_mut(&actor.local_id)
            .and_then(|entry| (entry.id == actor).then_some(entry))
    }

    fn actor_matches(state: &RuntimeState, actor: ActorId) -> bool {
        Self::actor_ref(state, actor).is_some()
    }

    fn is_idle_state(state: &RuntimeState) -> bool {
        state.actors.values().all(|actor| {
            actor.mailbox.is_empty()
                && matches!(actor.status, ActorStatus::Waiting | ActorStatus::Dead)
        })
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, RuntimeState> {
        self.state.lock().expect("runtime state poisoned")
    }

    fn allocate_id(state: &mut RuntimeState) -> ActorId {
        let id = ActorId::new(state.next_local_id, 0);
        state.next_local_id += 1;
        id
    }

    fn actor_scheduler(&self, actor: ActorId) -> usize {
        let scheduler_count = self.config.scheduler_count.max(1);
        actor.local_id as usize % scheduler_count
    }

    fn insert_actor_record(
        &self,
        state: &mut RuntimeState,
        actor: ActorRecord,
        link_to_parent: bool,
    ) -> Result<(), SpawnError> {
        if state.live_actors >= self.config.max_actors {
            return Err(SpawnError::CapacityExceeded);
        }

        let actor_id = actor.id;
        let parent = actor.parent;
        let supervisor_child = actor.supervisor_child;
        let registered_name = actor.registered_name.clone();
        let name = actor.name;

        if let Some(name) = actor.registered_name.clone() {
            state
                .registry
                .register(actor.id, name)
                .map_err(SpawnError::Registry)?;
        }

        state.actors.insert(actor.id.local_id, actor);
        state.live_actors += 1;

        if link_to_parent && let Some(parent_id) = parent {
            if let Some(parent_state) = state
                .actors
                .get_mut(&parent_id.local_id)
                .and_then(|entry| (entry.id == parent_id).then_some(entry))
            {
                parent_state.links.insert(actor_id);
            }

            if let Some(child_state) = state
                .actors
                .get_mut(&actor_id.local_id)
                .and_then(|entry| (entry.id == actor_id).then_some(entry))
            {
                child_state.links.insert(parent_id);
            }
        }

        if let (Some(parent), Some(child_id)) = (parent, supervisor_child)
            && let Some(parent_state) = state
                .actors
                .get_mut(&parent.local_id)
                .and_then(|entry| (entry.id == parent).then_some(entry))
            && let Some(supervisor) = parent_state.supervisor.as_mut()
        {
            supervisor.set_running(child_id, actor_id);
        }

        Self::record_lifecycle_event(
            state,
            LifecycleEvent::Spawn {
                actor: actor_id,
                name,
                registered_name,
                parent,
                supervisor_child,
            },
        );
        Ok(())
    }

    fn spawn_actor<A: Actor>(
        self: &Arc<Self>,
        actor: A,
        options: SpawnOptions,
    ) -> Result<ActorId, SpawnError> {
        let actor_id;
        let notify;
        {
            let mut state = self.lock_state();
            actor_id = Self::allocate_id(&mut state);
            let scheduler_id = self.actor_scheduler(actor_id);
            let record = ActorRecord::new(
                actor_id,
                actor.name(),
                options.clone(),
                &self.config,
                scheduler_id,
            );
            notify = Arc::clone(&record.notify);
            self.insert_actor_record(&mut state, record, options.link_to_parent)?;
        }

        let runtime = Arc::clone(self);
        drop(
            self.actor_handle
                .spawn(worker::run_actor_task(runtime, actor_id, Box::new(actor))),
        );
        notify.notify_one();
        self.idle_cv.notify_all();
        Ok(actor_id)
    }

    fn enqueue_actor_envelope(&self, actor: ActorId, envelope: Envelope) -> Result<(), SendError> {
        if let Envelope::Reply { reference, .. } = &envelope {
            self.cancel_call_timeout(actor, *reference);
        }

        let notify = {
            let mut state = self.lock_state();
            let Some(target) = Self::actor_mut(&mut state, actor) else {
                return Err(SendError::NoProc(actor));
            };

            push_envelope(target, envelope)?;
            Arc::clone(&target.notify)
        };

        notify.notify_one();
        self.idle_cv.notify_all();
        Ok(())
    }

    fn enqueue_runtime_envelope(
        &self,
        actor: ActorId,
        envelope: Envelope,
        label: &'static str,
    ) -> bool {
        match self.enqueue_actor_envelope(actor, envelope) {
            Ok(()) => true,
            Err(SendError::MailboxFull { .. }) => {
                self.request_force_exit(actor, mailbox_overflow_reason(actor, label))
            }
            Err(SendError::NoProc(_)) => false,
        }
    }

    fn request_force_exit(&self, actor: ActorId, reason: ExitReason) -> bool {
        let notify = {
            let mut state = self.lock_state();
            let Some(target) = Self::actor_mut(&mut state, actor) else {
                return false;
            };

            target.forced_exit = Some(reason);
            target.status = ActorStatus::Exiting;
            target.metrics.scheduler_id = Some(target.scheduler_id);
            Arc::clone(&target.notify)
        };

        notify.notify_one();
        self.idle_cv.notify_all();
        true
    }

    fn contains(&self, actor: ActorId) -> bool {
        let state = self.lock_state();
        Self::actor_matches(&state, actor)
    }

    fn resolve_name(&self, name: &str) -> Option<ActorId> {
        self.lock_state().registry.resolve(name)
    }

    fn cancel_actor_timer(&self, actor: ActorId, token: TimerToken) -> bool {
        let handle = {
            let mut state = self.lock_state();
            Self::actor_mut(&mut state, actor).and_then(|entry| entry.timer_tasks.remove(&token))
        };

        if let Some(handle) = handle {
            handle.abort();
            return true;
        }

        false
    }

    fn cancel_call_timeout(&self, actor: ActorId, reference: Ref) -> bool {
        let handle = {
            let mut state = self.lock_state();
            Self::actor_mut(&mut state, actor)
                .and_then(|entry| entry.call_timeout_tasks.remove(&reference))
        };

        if let Some(handle) = handle {
            handle.abort();
            return true;
        }

        false
    }

    fn spawn_timer(self: &Arc<Self>, actor: ActorId, token: TimerToken, delay: Duration) {
        self.cancel_actor_timer(actor, token);

        let runtime = Arc::clone(self);
        let handle = self.actor_handle.spawn(async move {
            tokio::time::sleep(delay).await;
            runtime.fire_timer(actor, token);
        });

        let mut state = self.lock_state();
        if let Some(entry) = state
            .actors
            .get_mut(&actor.local_id)
            .and_then(|record| (record.id == actor).then_some(record))
        {
            entry.timer_tasks.insert(token, handle);
        } else {
            handle.abort();
        }
    }

    fn spawn_call_timeout(self: &Arc<Self>, actor: ActorId, reference: Ref, delay: Duration) {
        self.cancel_call_timeout(actor, reference);

        let runtime = Arc::clone(self);
        let handle = self.actor_handle.spawn(async move {
            tokio::time::sleep(delay).await;
            runtime.fire_call_timeout(actor, reference);
        });

        let mut state = self.lock_state();
        if let Some(entry) = state
            .actors
            .get_mut(&actor.local_id)
            .and_then(|record| (record.id == actor).then_some(record))
        {
            entry.call_timeout_tasks.insert(reference, handle);
        } else {
            handle.abort();
        }
    }

    fn spawn_shutdown_timeout(self: &Arc<Self>, actor: ActorId, delay: Duration) -> JoinHandle<()> {
        let runtime = Arc::clone(self);
        self.actor_handle.spawn(async move {
            tokio::time::sleep(delay).await;
            runtime.fire_shutdown_timeout(actor);
        })
    }

    fn fire_timer(&self, actor: ActorId, token: TimerToken) {
        {
            let mut state = self.lock_state();
            let Some(entry) = state
                .actors
                .get_mut(&actor.local_id)
                .and_then(|record| (record.id == actor).then_some(record))
            else {
                return;
            };

            if entry.timer_tasks.remove(&token).is_none() {
                return;
            }
        }

        let _ =
            self.enqueue_runtime_envelope(actor, Envelope::Timer(TimerFired { token }), "timer");
    }

    fn fire_call_timeout(&self, actor: ActorId, reference: Ref) {
        {
            let mut state = self.lock_state();
            let Some(entry) = state
                .actors
                .get_mut(&actor.local_id)
                .and_then(|record| (record.id == actor).then_some(record))
            else {
                return;
            };

            if entry.call_timeout_tasks.remove(&reference).is_none() {
                return;
            }
        }

        let _ =
            self.enqueue_runtime_envelope(actor, Envelope::call_timeout(reference), "call timeout");
    }

    fn fire_shutdown_timeout(&self, actor: ActorId) {
        let tracker = {
            let mut state = self.lock_state();
            let Some(entry) = state
                .actors
                .get_mut(&actor.local_id)
                .and_then(|record| (record.id == actor).then_some(record))
            else {
                return;
            };

            let Some(tracker) = entry.shutdown.as_ref() else {
                return;
            };

            (tracker.requester, tracker.policy.clone())
        };

        let mut state = self.lock_state();
        Self::record_lifecycle_event(
            &mut state,
            LifecycleEvent::Shutdown {
                requester: tracker.0,
                actor,
                policy: tracker.1,
                phase: ShutdownPhase::TimedOut,
                reason: Some(ExitReason::Kill),
            },
        );
        drop(state);

        let _ = self.request_force_exit(actor, ExitReason::Kill);
    }

    fn register_name(&self, actor: ActorId, name: String) -> Result<(), RegistryError> {
        let mut state = self.lock_state();
        if !Self::actor_matches(&state, actor) {
            return Err(RegistryError::NoProc(actor));
        }

        state.registry.register(actor, name.clone())?;
        if let Some(entry) = Self::actor_mut(&mut state, actor) {
            entry.registered_name = Some(name);
        }

        Ok(())
    }

    fn unregister_name(&self, actor: ActorId) -> Option<String> {
        let mut state = self.lock_state();
        let removed = state.registry.unregister(actor)?;
        if let Some(entry) = Self::actor_mut(&mut state, actor) {
            entry.registered_name = None;
        }
        Some(removed)
    }

    fn supervisor_snapshot(&self, actor: ActorId) -> Option<SupervisorSnapshot> {
        let mut state = self.lock_state();
        Self::actor_mut(&mut state, actor).and_then(|entry| {
            entry
                .supervisor
                .as_mut()
                .map(|supervisor| supervisor.snapshot(actor))
        })
    }

    fn lifecycle_events(&self) -> Vec<LifecycleEvent> {
        self.lock_state().lifecycle_events.clone()
    }

    fn crash_reports(&self) -> Vec<CrashReport> {
        self.lock_state().crash_reports.clone()
    }

    fn actor_snapshot(&self, actor: ActorId) -> Option<ActorSnapshot> {
        let state = self.lock_state();
        Self::actor_ref(&state, actor)
            .map(ActorRecord::snapshot)
            .or_else(|| state.completed.get(&actor).cloned())
    }

    fn actor_snapshots(&self) -> Vec<ActorSnapshot> {
        let state = self.lock_state();
        let mut snapshots: Vec<_> = state.actors.values().map(ActorRecord::snapshot).collect();
        snapshots.sort_by_key(|snapshot| snapshot.id);
        snapshots
    }

    fn event_log(&self) -> Vec<RuntimeEvent> {
        self.lock_state().event_log.clone()
    }

    fn event_cursor(&self) -> EventCursor {
        EventCursor::new(self.lock_state().next_event_sequence)
    }

    fn events_since(&self, cursor: &mut EventCursor) -> Vec<RuntimeEvent> {
        let state = self.lock_state();
        events_since(&state.event_log, cursor)
    }

    fn actor_identity_from_state(state: &RuntimeState, actor: ActorId) -> Option<ActorIdentity> {
        Self::actor_ref(state, actor)
            .map(ActorRecord::snapshot)
            .or_else(|| state.completed.get(&actor).cloned())
            .map(|snapshot| {
                ActorIdentity::new(snapshot.id, snapshot.name, snapshot.registered_name.clone())
            })
    }

    fn record_runtime_event(state: &mut RuntimeState, kind: RuntimeEventKind) {
        let record = RuntimeEvent {
            sequence: state.next_event_sequence,
            emitted_at: std::time::SystemTime::now(),
            kind,
        };
        state.next_event_sequence += 1;
        emit_tracing_event(&record);
        state.event_log.push(record);
    }

    fn record_lifecycle_event(state: &mut RuntimeState, event: LifecycleEvent) {
        state.lifecycle_events.push(event.clone());
        Self::record_runtime_event(state, RuntimeEventKind::Lifecycle(event));
    }

    fn record_crash_report(state: &mut RuntimeState, report: CrashReport) {
        state.crash_reports.push(report.clone());
        Self::record_runtime_event(state, RuntimeEventKind::Crash(report));
    }
}

struct ConcurrentContext {
    runtime: Arc<RuntimeShared>,
    actor_id: ActorId,
    effects: TurnEffects,
}

impl ConcurrentContext {
    fn new(runtime: Arc<RuntimeShared>, actor_id: ActorId) -> Self {
        Self {
            runtime,
            actor_id,
            effects: TurnEffects::default(),
        }
    }

    fn finish(self) -> TurnEffects {
        self.effects
    }

    fn actor_record<F, R>(&self, op: F) -> R
    where
        F: FnOnce(&ActorRecord) -> R,
    {
        let state = self.runtime.lock_state();
        let actor = RuntimeShared::actor_ref(&state, self.actor_id)
            .expect("current actor must exist while running");
        op(actor)
    }

    fn actor_record_mut<F, R>(&self, op: F) -> R
    where
        F: FnOnce(&mut ActorRecord) -> R,
    {
        let mut state = self.runtime.lock_state();
        let actor = RuntimeShared::actor_mut(&mut state, self.actor_id)
            .expect("current actor must exist while running");
        op(actor)
    }

    fn send_to(&mut self, to: ActorId, envelope: Envelope) -> Result<(), SendError> {
        self.runtime.enqueue_actor_envelope(to, envelope)
    }

    fn enqueue_self_runtime_envelope(&mut self, envelope: Envelope, label: &'static str) {
        let result = self.actor_record_mut(|actor| push_envelope(actor, envelope));
        match result {
            Ok(()) => {
                let notify = self.actor_record(|actor| Arc::clone(&actor.notify));
                notify.notify_one();
            }
            Err(SendError::MailboxFull { .. }) => {
                self.effects.exit_reason = Some(mailbox_overflow_reason(self.actor_id, label));
            }
            Err(SendError::NoProc(_)) => {}
        }
    }

    fn spawn_inner<A: Actor>(
        &mut self,
        actor: A,
        mut options: SpawnOptions,
    ) -> Result<ActorId, SpawnError> {
        if options.parent.is_none() {
            options.parent = Some(self.actor_id);
        }

        if options.ancestors.is_empty() {
            let ancestors = self.actor_record(|actor| actor.ancestors.clone());
            options.ancestors = ancestors;
            options.ancestors.push(self.actor_id);
        }

        self.runtime.spawn_actor(actor, options)
    }

    fn link_inner(&mut self, other: ActorId) -> Result<(), LinkError> {
        if other == self.actor_id {
            return Ok(());
        }

        let mut state = self.runtime.lock_state();
        if !RuntimeShared::actor_matches(&state, self.actor_id) {
            return Err(LinkError::NoProc(self.actor_id));
        }
        if !RuntimeShared::actor_matches(&state, other) {
            return Err(LinkError::NoProc(other));
        }

        let current = state
            .actors
            .get_mut(&self.actor_id.local_id)
            .expect("current actor must exist");
        if !current.links.insert(other) {
            return Err(LinkError::AlreadyLinked(other));
        }
        state
            .actors
            .get_mut(&other.local_id)
            .expect("target actor must exist")
            .links
            .insert(self.actor_id);
        Ok(())
    }

    fn unlink_inner(&mut self, other: ActorId) -> Result<(), LinkError> {
        if other == self.actor_id {
            return Ok(());
        }

        let mut state = self.runtime.lock_state();
        if !RuntimeShared::actor_matches(&state, self.actor_id) {
            return Err(LinkError::NoProc(self.actor_id));
        }
        if !RuntimeShared::actor_matches(&state, other) {
            return Err(LinkError::NoProc(other));
        }

        if !state
            .actors
            .get_mut(&self.actor_id.local_id)
            .expect("current actor must exist")
            .links
            .remove(&other)
        {
            return Err(LinkError::NotLinked(other));
        }

        state
            .actors
            .get_mut(&other.local_id)
            .expect("target actor must exist")
            .links
            .remove(&self.actor_id);
        Ok(())
    }

    fn monitor_inner(&mut self, other: ActorId) -> Ref {
        let reference = Ref::next();
        if other == self.actor_id {
            self.actor_record_mut(|actor| {
                actor.monitors_in.insert(reference, self.actor_id);
                actor.monitors_out.insert(reference, self.actor_id);
            });
            return reference;
        }

        let mut state = self.runtime.lock_state();
        if state
            .actors
            .get(&other.local_id)
            .is_some_and(|entry| entry.id == other)
        {
            state
                .actors
                .get_mut(&self.actor_id.local_id)
                .expect("current actor must exist")
                .monitors_out
                .insert(reference, other);
            state
                .actors
                .get_mut(&other.local_id)
                .expect("target actor must exist")
                .monitors_in
                .insert(reference, self.actor_id);
        } else {
            drop(state);
            self.enqueue_self_runtime_envelope(
                Envelope::Down(DownMessage {
                    reference,
                    actor: other,
                    reason: ExitReason::NoProc,
                }),
                "down",
            );
        }

        reference
    }

    fn demonitor_inner(&mut self, reference: Ref) -> Result<(), MonitorError> {
        let mut state = self.runtime.lock_state();
        let current = state
            .actors
            .get_mut(&self.actor_id.local_id)
            .and_then(|entry| (entry.id == self.actor_id).then_some(entry))
            .ok_or(MonitorError::NoProc(self.actor_id))?;

        let Some(target) = current.monitors_out.remove(&reference) else {
            return Err(MonitorError::UnknownRef(reference));
        };

        if target == self.actor_id {
            current.monitors_in.remove(&reference);
            return Ok(());
        }

        if let Some(target_state) = state
            .actors
            .get_mut(&target.local_id)
            .and_then(|entry| (entry.id == target).then_some(entry))
        {
            target_state.monitors_in.remove(&reference);
        }

        Ok(())
    }

    fn spawn_blocking_task<F, R>(&mut self, pool: PoolKind, job: F) -> TaskHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        {
            let mut state = self.runtime.lock_state();
            match pool {
                PoolKind::BlockingIo => state.metrics.blocking_io_jobs += 1,
                PoolKind::BlockingCpu => state.metrics.blocking_cpu_jobs += 1,
                PoolKind::Normal => {}
            }
        }

        let id = Ref::next();
        let target = self.actor_id;
        let runtime = self.runtime.clone();
        match pool {
            PoolKind::BlockingIo | PoolKind::Normal => {
                drop(self.runtime.actor_handle.spawn_blocking(move || {
                    let result = job();
                    let _ = runtime.enqueue_runtime_envelope(
                        target,
                        Envelope::Task(TaskCompleted {
                            reference: id,
                            pool,
                            result: Payload::new(result),
                        }),
                        "task",
                    );
                }));
            }
            PoolKind::BlockingCpu => {
                drop(self.runtime.cpu_handle.spawn(async move {
                    let result = job();
                    let _ = runtime.enqueue_runtime_envelope(
                        target,
                        Envelope::Task(TaskCompleted {
                            reference: id,
                            pool,
                            result: Payload::new(result),
                        }),
                        "task",
                    );
                }));
            }
        }

        TaskHandle::new(id, pool)
    }
}

impl ActorContext for ConcurrentContext {
    fn actor_id(&self) -> ActorId {
        self.actor_id
    }

    fn scheduler_id(&self) -> Option<usize> {
        Some(self.actor_record(|actor| actor.scheduler_id))
    }

    fn spawn<A: Actor>(&mut self, actor: A, options: SpawnOptions) -> Result<ActorId, SpawnError> {
        self.spawn_inner(actor, options)
    }

    fn whereis(&self, name: &str) -> Option<ActorId> {
        self.runtime.resolve_name(name)
    }

    fn register_name(&mut self, name: String) -> Result<(), RegistryError> {
        self.runtime.register_name(self.actor_id, name)
    }

    fn unregister_name(&mut self) -> Option<String> {
        self.runtime.unregister_name(self.actor_id)
    }

    fn send_envelope(&mut self, to: ActorId, envelope: Envelope) -> Result<(), SendError> {
        self.send_to(to, envelope)
    }

    fn ask<M: Message>(
        &mut self,
        to: ActorId,
        message: M,
        timeout: Option<Duration>,
    ) -> Result<PendingCall, SendError> {
        let reference = Ref::next();
        let reply_to = ReplyToken::new(self.actor_id, reference);
        let watermark = self.actor_record(|actor| actor.mailbox.watermark());
        self.send_to(to, Envelope::request(reply_to, message))?;
        if let Some(timeout) = timeout {
            self.runtime
                .spawn_call_timeout(self.actor_id, reference, timeout);
        }
        Ok(PendingCall::new(reply_to, watermark, timeout))
    }

    fn link(&mut self, other: ActorId) -> Result<(), LinkError> {
        self.link_inner(other)
    }

    fn unlink(&mut self, other: ActorId) -> Result<(), LinkError> {
        self.unlink_inner(other)
    }

    fn monitor(&mut self, other: ActorId) -> Result<Ref, MonitorError> {
        Ok(self.monitor_inner(other))
    }

    fn demonitor(&mut self, reference: Ref) -> Result<(), MonitorError> {
        self.demonitor_inner(reference)
    }

    fn set_trap_exit(&mut self, enabled: bool) {
        self.actor_record_mut(|actor| actor.trap_exit = enabled);
    }

    fn schedule_after(&mut self, delay: Duration, token: TimerToken) -> Result<(), TimerError> {
        self.runtime.spawn_timer(self.actor_id, token, delay);
        Ok(())
    }

    fn cancel_timer(&mut self, token: TimerToken) -> bool {
        self.runtime.cancel_actor_timer(self.actor_id, token)
    }

    fn spawn_blocking_io<F, R>(&mut self, job: F) -> TaskHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        self.spawn_blocking_task(PoolKind::BlockingIo, job)
    }

    fn spawn_blocking_cpu<F, R>(&mut self, job: F) -> TaskHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        self.spawn_blocking_task(PoolKind::BlockingCpu, job)
    }

    fn yield_now(&mut self) {
        self.effects.yielded = true;
    }

    fn exit(&mut self, reason: ExitReason) {
        self.effects.exit_reason = Some(reason);
    }

    fn shutdown_actor(&mut self, actor: ActorId, policy: Shutdown) -> Result<(), SendError> {
        if !self.runtime.contains(actor) {
            return Err(SendError::NoProc(actor));
        }

        {
            let mut state = self.runtime.lock_state();
            if !RuntimeShared::actor_matches(&state, actor) {
                return Err(SendError::NoProc(actor));
            }

            let prior = RuntimeShared::actor_mut(&mut state, actor)
                .expect("target actor must exist")
                .shutdown
                .take();
            if let Some(mut tracker) = prior
                && let Some(task) = tracker.task.take()
            {
                task.abort();
            }

            RuntimeShared::record_lifecycle_event(
                &mut state,
                LifecycleEvent::Shutdown {
                    requester: self.actor_id,
                    actor,
                    policy: policy.clone(),
                    phase: ShutdownPhase::Requested,
                    reason: None,
                },
            );

            let task = match policy.clone() {
                Shutdown::BrutalKill => None,
                Shutdown::Timeout(delay) => Some(self.runtime.spawn_shutdown_timeout(actor, delay)),
                Shutdown::Infinity => None,
            };

            RuntimeShared::actor_mut(&mut state, actor)
                .expect("target actor must exist")
                .shutdown = Some(ShutdownTracker {
                requester: self.actor_id,
                policy: policy.clone(),
                task,
            });
        }

        match policy {
            Shutdown::BrutalKill => {
                self.runtime.request_force_exit(actor, ExitReason::Kill);
            }
            Shutdown::Timeout(_) | Shutdown::Infinity => {
                self.runtime.propagate_link_exit(
                    actor,
                    ExitSignal {
                        from: self.actor_id,
                        reason: ExitReason::Shutdown,
                        linked: true,
                    },
                );
            }
        }

        Ok(())
    }
}

impl SupervisorContext for ConcurrentContext {
    fn configure_supervisor(&mut self, flags: SupervisorFlags, child_specs: Vec<ChildSpec>) {
        self.actor_record_mut(|actor| {
            actor.supervisor = Some(SupervisorRuntimeState::new(flags, child_specs));
        });
    }

    fn record_supervisor_restart(&mut self) -> bool {
        self.actor_record_mut(|actor| {
            let allowed = actor
                .supervisor
                .as_mut()
                .is_none_or(|supervisor| supervisor.intensity.record_restart(Instant::now()));

            if allowed {
                actor.metrics.restarts += 1;
            }

            allowed
        })
    }

    fn supervisor_child_started(&mut self, child_id: &'static str, actor: ActorId) {
        self.actor_record_mut(|current| {
            if let Some(supervisor) = current.supervisor.as_mut() {
                supervisor.set_running(child_id, actor);
            }
        });
    }

    fn supervisor_child_exited(&mut self, child_id: &'static str, actor: ActorId) {
        self.actor_record_mut(|current| {
            if let Some(supervisor) = current.supervisor.as_mut() {
                supervisor.clear_running(child_id, actor);
            }
        });
    }
}

impl LifecycleContext for ConcurrentContext {
    fn emit_lifecycle_event(&mut self, event: LifecycleEvent) {
        let mut state = self.runtime.lock_state();
        RuntimeShared::record_lifecycle_event(&mut state, event);
    }
}

fn push_envelope(state: &mut ActorRecord, envelope: Envelope) -> Result<(), SendError> {
    let MailboxFull { capacity } = match state.mailbox.try_push(envelope) {
        Ok(()) => {
            state.metrics.mailbox_len = state.mailbox.len();
            if !matches!(state.status, ActorStatus::Dead | ActorStatus::Exiting)
                && matches!(state.status, ActorStatus::Waiting)
            {
                state.status = ActorStatus::Runnable;
            }
            return Ok(());
        }
        Err(error) => error,
    };

    state.metrics.mailbox_len = state.mailbox.len();
    Err(SendError::MailboxFull {
        actor: state.id,
        capacity,
    })
}

/// A Tokio-backed multicore runtime that preserves Lamport's mailbox and supervision semantics.
pub struct ConcurrentRuntime {
    config: SchedulerConfig,
    actor_runtime: Option<TokioRuntime>,
    cpu_runtime: Option<TokioRuntime>,
    inner: Arc<RuntimeShared>,
}

impl Default for ConcurrentRuntime {
    fn default() -> Self {
        Self::new(SchedulerConfig::default())
    }
}

impl ConcurrentRuntime {
    /// Creates a new runtime and returns any Tokio initialization error.
    pub fn try_new(config: SchedulerConfig) -> Result<Self, std::io::Error> {
        let config = normalize_scheduler_config(config);

        let actor_runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(config.scheduler_count)
            .max_blocking_threads(config.blocking_io_threads)
            .enable_time()
            .thread_name("lamport-actor")
            .build()?;
        let actor_handle = actor_runtime.handle().clone();

        let cpu_runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(config.blocking_cpu_threads)
            .thread_name("lamport-dirty-cpu")
            .build()?;
        let cpu_handle = cpu_runtime.handle().clone();

        let inner = Arc::new(RuntimeShared {
            config: config.clone(),
            actor_handle,
            cpu_handle,
            state: Mutex::new(RuntimeState {
                next_event_sequence: 1,
                ..RuntimeState::default()
            }),
            idle_cv: Condvar::new(),
            started_at: Instant::now(),
        });

        Ok(Self {
            config,
            actor_runtime: Some(actor_runtime),
            cpu_runtime: Some(cpu_runtime),
            inner,
        })
    }

    /// Creates a new runtime with a multicore Tokio scheduler.
    pub fn new(config: SchedulerConfig) -> Self {
        Self::try_new(config).expect("failed to initialize Tokio runtimes for lamport")
    }

    /// Boots an application root supervisor into an existing concurrent runtime.
    pub fn boot_application<A: crate::application::Application>(
        &self,
        application: A,
    ) -> Result<crate::application::ApplicationHandle, SpawnError> {
        let (name, options, root_supervisor) = crate::application::prepare_application(application);
        let root = self.spawn_supervisor_with_options(root_supervisor, options)?;
        Ok(crate::application::ApplicationHandle::new(name, root))
    }

    /// Returns the scheduler configuration used by the runtime.
    pub fn config(&self) -> &SchedulerConfig {
        &self.config
    }

    /// Spawns a new actor with default options.
    pub fn spawn<A: Actor>(&self, actor: A) -> Result<ActorId, SpawnError> {
        self.spawn_with_options(actor, SpawnOptions::default())
    }

    /// Spawns a new actor with explicit options.
    pub fn spawn_with_options<A: Actor>(
        &self,
        actor: A,
        options: SpawnOptions,
    ) -> Result<ActorId, SpawnError> {
        self.inner.spawn_actor(actor, options)
    }

    /// Spawns a typed `GenServer` with default options.
    pub fn spawn_gen_server<G>(&self, server: G) -> Result<ActorId, SpawnError>
    where
        G: GenServer,
        G::Info: From<RuntimeInfo>,
        G::State: Send,
    {
        self.spawn_gen_server_with_options(server, SpawnOptions::default())
    }

    /// Spawns a typed `GenServer` with explicit options.
    pub fn spawn_gen_server_with_options<G>(
        &self,
        server: G,
        options: SpawnOptions,
    ) -> Result<ActorId, SpawnError>
    where
        G: GenServer,
        G::Info: From<RuntimeInfo>,
        G::State: Send,
    {
        self.spawn_with_options(GenServerActor::new(server), options)
    }

    /// Spawns a typed `GenStatem` with default options.
    pub fn spawn_gen_statem<G>(&self, machine: G) -> Result<ActorId, SpawnError>
    where
        G: GenStatem,
        G::Info: From<RuntimeInfo>,
    {
        self.spawn_gen_statem_with_options(machine, SpawnOptions::default())
    }

    /// Spawns a typed `GenStatem` with explicit options.
    pub fn spawn_gen_statem_with_options<G>(
        &self,
        machine: G,
        options: SpawnOptions,
    ) -> Result<ActorId, SpawnError>
    where
        G: GenStatem,
        G::Info: From<RuntimeInfo>,
    {
        self.spawn_with_options(GenStatemActor::new(machine), options)
    }

    /// Spawns a typed `Supervisor` with default options.
    pub fn spawn_supervisor<S>(&self, supervisor: S) -> Result<ActorId, SpawnError>
    where
        S: Supervisor,
    {
        self.spawn_supervisor_with_options(supervisor, SpawnOptions::default())
    }

    /// Spawns a typed `Supervisor` with explicit options.
    pub fn spawn_supervisor_with_options<S>(
        &self,
        supervisor: S,
        options: SpawnOptions,
    ) -> Result<ActorId, SpawnError>
    where
        S: Supervisor,
    {
        self.spawn_with_options(SupervisorActor::new(supervisor), options)
    }

    /// Sends a typed message from outside the runtime.
    pub fn send<M: Message>(&self, to: ActorId, message: M) -> Result<(), SendError> {
        self.send_envelope(to, Envelope::user(message))
    }

    /// Sends a raw envelope from outside the runtime.
    pub fn send_envelope(&self, to: ActorId, envelope: Envelope) -> Result<(), SendError> {
        self.inner.enqueue_actor_envelope(to, envelope)
    }

    /// Requests an actor to exit at the next scheduling boundary.
    pub fn exit_actor(&self, actor: ActorId, reason: ExitReason) -> bool {
        self.inner.request_force_exit(actor, reason)
    }

    /// Returns the current or completed snapshot for an actor.
    pub fn actor_snapshot(&self, actor: ActorId) -> Option<ActorSnapshot> {
        self.inner.actor_snapshot(actor)
    }

    /// Returns the tracked supervisor state for a live supervisor actor.
    pub fn supervisor_snapshot(&self, actor: ActorId) -> Option<SupervisorSnapshot> {
        self.inner.supervisor_snapshot(actor)
    }

    /// Returns the accumulated runtime lifecycle events.
    pub fn lifecycle_events(&self) -> Vec<LifecycleEvent> {
        self.inner.lifecycle_events()
    }

    /// Returns the accumulated abnormal crash reports.
    pub fn crash_reports(&self) -> Vec<CrashReport> {
        self.inner.crash_reports()
    }

    /// Returns retained structured runtime events for tracing and debugging.
    pub fn event_log(&self) -> Vec<RuntimeEvent> {
        self.inner.event_log()
    }

    /// Returns a cursor positioned after the current retained event log.
    pub fn event_cursor(&self) -> EventCursor {
        self.inner.event_cursor()
    }

    /// Returns events emitted after the cursor and advances it.
    pub fn events_since(&self, cursor: &mut EventCursor) -> Vec<RuntimeEvent> {
        self.inner.events_since(cursor)
    }

    /// Returns snapshots for all live actors.
    pub fn actor_snapshots(&self) -> Vec<ActorSnapshot> {
        self.inner.actor_snapshots()
    }

    /// Returns the current live actor tree.
    pub fn actor_tree(&self) -> ActorTree {
        build_actor_tree(&self.actor_snapshots())
    }

    /// Returns an aggregate metrics snapshot suitable for production monitoring.
    pub fn metrics_snapshot(&self) -> RuntimeMetricsSnapshot {
        let actors = self.actor_snapshots();
        build_metrics_snapshot(
            &actors,
            self.completed_actor_count(),
            self.metrics(),
            self.run_queue_snapshots(),
        )
    }

    /// Exports runtime metrics in Prometheus text exposition format.
    pub fn export_metrics_prometheus(&self) -> String {
        self.metrics_snapshot().to_prometheus()
    }

    /// Returns a combined runtime introspection snapshot.
    pub fn introspection(&self) -> RuntimeIntrospection {
        build_runtime_introspection(
            self.actor_snapshots(),
            self.completed_actor_count(),
            self.metrics(),
            self.run_queue_snapshots(),
        )
    }

    /// Looks up a registered actor by name.
    pub fn resolve_name(&self, name: &str) -> Option<ActorId> {
        self.inner.resolve_name(name)
    }

    /// Registers a live actor under the provided name.
    pub fn register_name(&self, actor: ActorId, name: String) -> Result<(), RegistryError> {
        self.inner.register_name(actor, name)
    }

    /// Removes the current registered name for a live actor.
    pub fn unregister_name(&self, actor: ActorId) -> Option<String> {
        self.inner.unregister_name(actor)
    }

    /// Returns whether the actor id refers to a live actor.
    pub fn contains(&self, actor: ActorId) -> bool {
        self.inner.contains(actor)
    }

    fn completed_actor_count(&self) -> usize {
        self.inner.lock_state().completed.len()
    }

    /// Waits until all live actors are parked with empty mailboxes.
    pub fn wait_for_idle(&self, timeout: Option<Duration>) -> bool {
        let mut state = self.inner.lock_state();
        let start = Instant::now();
        loop {
            if RuntimeShared::is_idle_state(&state) {
                return true;
            }

            match timeout {
                Some(timeout) => {
                    let elapsed = start.elapsed();
                    if elapsed >= timeout {
                        return false;
                    }
                    let remaining = timeout.saturating_sub(elapsed);
                    let (next_state, wait) = self
                        .inner
                        .idle_cv
                        .wait_timeout(state, remaining)
                        .expect("runtime state poisoned");
                    state = next_state;
                    if wait.timed_out() {
                        return RuntimeShared::is_idle_state(&state);
                    }
                }
                None => {
                    state = self
                        .inner
                        .idle_cv
                        .wait(state)
                        .expect("runtime state poisoned");
                }
            }
        }
    }

    /// Returns scheduler snapshots for each Tokio worker.
    pub fn run_queue_snapshots(&self) -> Vec<RunQueueSnapshot> {
        let runtime_metrics = self.inner.actor_handle.metrics();
        let worker_count = runtime_metrics.num_workers();
        let mut runnable = vec![0_usize; worker_count];
        let mut waiting = vec![0_usize; worker_count];

        {
            let state = self.inner.lock_state();
            for actor in state.actors.values() {
                let scheduler_id = actor.scheduler_id.min(worker_count.saturating_sub(1));
                match actor.status {
                    ActorStatus::Starting | ActorStatus::Runnable | ActorStatus::Running => {
                        runnable[scheduler_id] += 1;
                    }
                    ActorStatus::Waiting => waiting[scheduler_id] += 1,
                    ActorStatus::Exiting | ActorStatus::Dead => {}
                }
            }
        }

        (0..worker_count)
            .map(|scheduler_id| RunQueueSnapshot {
                scheduler_id,
                runnable: runnable[scheduler_id],
                waiting: waiting[scheduler_id],
                injected: if scheduler_id == 0 {
                    runtime_metrics.global_queue_depth() as u64
                } else {
                    0
                },
                stolen: 0,
            })
            .collect()
    }

    /// Returns aggregate scheduler metrics.
    pub fn metrics(&self) -> SchedulerMetrics {
        let mut metrics = {
            let state = self.inner.lock_state();
            state.metrics
        };

        let runtime_metrics = self.inner.actor_handle.metrics();
        let workers = runtime_metrics.num_workers().max(1);
        let total_busy = (0..workers).fold(Duration::ZERO, |sum, worker| {
            sum.saturating_add(runtime_metrics.worker_total_busy_duration(worker))
        });
        let elapsed = self.inner.started_at.elapsed().as_secs_f32();
        let denominator = elapsed * workers as f32;
        metrics.utilization = if denominator > 0.0 {
            (total_busy.as_secs_f32() / denominator).clamp(0.0, 1.0)
        } else {
            0.0
        };
        metrics
    }
}

impl Scheduler for ConcurrentRuntime {
    fn config(&self) -> &SchedulerConfig {
        &self.config
    }

    fn spawn<A: Actor>(&self, actor: A, options: SpawnOptions) -> Result<ActorId, SpawnError> {
        self.spawn_with_options(actor, options)
    }

    fn enqueue(&self, actor: ActorId) -> Result<(), ScheduleError> {
        let notify = {
            let state = self.inner.lock_state();
            let Some(target) = state
                .actors
                .get(&actor.local_id)
                .and_then(|entry| (entry.id == actor).then_some(entry))
            else {
                return Err(ScheduleError::NoProc(actor));
            };
            Arc::clone(&target.notify)
        };
        notify.notify_one();
        Ok(())
    }

    fn wake(&self, actor: ActorId) -> Result<(), ScheduleError> {
        self.enqueue(actor)
    }

    fn park(&self, actor: ActorId) {
        let mut state = self.inner.lock_state();
        if let Some(entry) = state
            .actors
            .get_mut(&actor.local_id)
            .and_then(|record| (record.id == actor).then_some(record))
            && entry.mailbox.is_empty()
        {
            entry.status = ActorStatus::Waiting;
            entry.metrics.scheduler_id = None;
        }
        self.inner.idle_cv.notify_all();
    }

    fn run_queue_snapshots(&self) -> Vec<RunQueueSnapshot> {
        self.run_queue_snapshots()
    }

    fn metrics(&self) -> SchedulerMetrics {
        self.metrics()
    }
}

impl Drop for ConcurrentRuntime {
    fn drop(&mut self) {
        if let Some(runtime) = self.cpu_runtime.take() {
            runtime.shutdown_background();
        }
        if let Some(runtime) = self.actor_runtime.take() {
            runtime.shutdown_background();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
            mpsc as std_mpsc,
        },
        time::{Duration, Instant},
    };

    use crate::{
        Actor, ActorTurn, Context, Envelope, ExitReason, LifecycleEvent, PoolKind, Ref,
        SchedulerConfig, SendError, SpawnError, SpawnOptions, TimerToken,
        observability::RuntimeEventKind,
    };

    use super::ConcurrentRuntime;

    struct SinkActor;

    impl Actor for SinkActor {
        fn handle<C: Context>(&mut self, _envelope: Envelope, _ctx: &mut C) -> ActorTurn {
            ActorTurn::Continue
        }
    }

    struct SerialActor {
        active: Arc<AtomicUsize>,
        max_seen: Arc<AtomicUsize>,
        completed: Arc<AtomicUsize>,
    }

    impl Actor for SerialActor {
        fn handle<C: Context>(&mut self, _envelope: Envelope, _ctx: &mut C) -> ActorTurn {
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_seen.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(30));
            self.active.fetch_sub(1, Ordering::SeqCst);
            self.completed.fetch_add(1, Ordering::SeqCst);
            ActorTurn::Continue
        }
    }

    struct SlowActor {
        sender: Option<std_mpsc::Sender<Instant>>,
    }

    impl Actor for SlowActor {
        fn handle<C: Context>(&mut self, _envelope: Envelope, _ctx: &mut C) -> ActorTurn {
            if let Some(sender) = self.sender.take() {
                sender.send(Instant::now()).unwrap();
            }
            std::thread::sleep(Duration::from_millis(120));
            ActorTurn::Stop(ExitReason::Normal)
        }
    }

    struct TimerActor {
        token: TimerToken,
        seen: Arc<Mutex<Vec<TimerToken>>>,
    }

    impl Actor for TimerActor {
        fn init<C: Context>(&mut self, ctx: &mut C) -> Result<(), ExitReason> {
            ctx.schedule_after(Duration::from_millis(10), self.token)
                .map_err(|error| ExitReason::Error(format!("schedule failed: {error:?}")))?;
            Ok(())
        }

        fn handle<C: Context>(&mut self, envelope: Envelope, _ctx: &mut C) -> ActorTurn {
            if let Envelope::Timer(timer) = envelope {
                self.seen.lock().unwrap().push(timer.token);
            }

            ActorTurn::Continue
        }
    }

    struct BlockingActor {
        seen: Arc<Mutex<Vec<(Ref, PoolKind, i32)>>>,
        expected: Option<Ref>,
    }

    impl Actor for BlockingActor {
        fn init<C: Context>(&mut self, ctx: &mut C) -> Result<(), ExitReason> {
            let handle = ctx.spawn_blocking_cpu(|| 42_i32);
            self.expected = Some(handle.id());
            Ok(())
        }

        fn handle<C: Context>(&mut self, envelope: Envelope, _ctx: &mut C) -> ActorTurn {
            if let Envelope::Task(task) = envelope {
                let value = task.result.downcast::<i32>().ok().unwrap();
                self.seen
                    .lock()
                    .unwrap()
                    .push((task.reference, task.pool, value));
                assert_eq!(Some(task.reference), self.expected);
            }

            ActorTurn::Continue
        }
    }

    struct NeverReplyActor;

    impl Actor for NeverReplyActor {
        fn handle<C: Context>(&mut self, _envelope: Envelope, _ctx: &mut C) -> ActorTurn {
            ActorTurn::Continue
        }
    }

    struct TimeoutClient {
        server: crate::ActorId,
        seen: Arc<Mutex<Vec<Ref>>>,
        pending: Option<crate::PendingCall>,
    }

    impl Actor for TimeoutClient {
        fn init<C: Context>(&mut self, ctx: &mut C) -> Result<(), ExitReason> {
            self.pending = Some(
                ctx.ask(self.server, "ping", Some(Duration::from_millis(10)))
                    .map_err(|error| ExitReason::Error(format!("ask failed: {error:?}")))?,
            );
            Ok(())
        }

        fn handle<C: Context>(&mut self, envelope: Envelope, _ctx: &mut C) -> ActorTurn {
            if let Envelope::CallTimeout(timeout) = envelope
                && self
                    .pending
                    .is_some_and(|pending| pending.matches(timeout.reference))
            {
                self.seen.lock().unwrap().push(timeout.reference);
            }

            ActorTurn::Continue
        }
    }

    #[test]
    fn concurrent_runtime_processes_different_actors_in_parallel() {
        let runtime = ConcurrentRuntime::new(SchedulerConfig {
            scheduler_count: 2,
            ..SchedulerConfig::default()
        });
        let (first_tx, first_rx) = std_mpsc::channel();
        let (second_tx, second_rx) = std_mpsc::channel();

        let first = runtime
            .spawn(SlowActor {
                sender: Some(first_tx),
            })
            .unwrap();
        let second = runtime
            .spawn(SlowActor {
                sender: Some(second_tx),
            })
            .unwrap();

        runtime.send(first, ()).unwrap();
        runtime.send(second, ()).unwrap();

        let first_started = first_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let second_started = second_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let delta = if first_started >= second_started {
            first_started.duration_since(second_started)
        } else {
            second_started.duration_since(first_started)
        };

        assert!(delta < Duration::from_millis(80));
    }

    #[test]
    fn one_actor_never_runs_more_than_one_turn_at_a_time() {
        let runtime = ConcurrentRuntime::default();
        let active = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(AtomicUsize::new(0));
        let actor = runtime
            .spawn(SerialActor {
                active: Arc::clone(&active),
                max_seen: Arc::clone(&max_seen),
                completed: Arc::clone(&completed),
            })
            .unwrap();

        for _ in 0..4 {
            runtime.send(actor, ()).unwrap();
        }

        let deadline = Instant::now() + Duration::from_secs(2);
        while completed.load(Ordering::SeqCst) < 4 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }

        assert_eq!(completed.load(Ordering::SeqCst), 4);
        assert_eq!(max_seen.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn bounded_mailbox_returns_backpressure() {
        let runtime = ConcurrentRuntime::default();
        let actor = runtime
            .spawn_with_options(
                SinkActor,
                SpawnOptions {
                    mailbox_capacity: Some(1),
                    ..SpawnOptions::default()
                },
            )
            .unwrap();

        runtime.send(actor, 1_u32).unwrap();
        assert_eq!(
            runtime.send(actor, 2_u32),
            Err(SendError::MailboxFull { actor, capacity: 1 })
        );
    }

    #[test]
    fn actor_capacity_limit_rejects_new_spawns() {
        let runtime = ConcurrentRuntime::new(SchedulerConfig {
            max_actors: 1,
            ..SchedulerConfig::default()
        });

        runtime.spawn(SinkActor).unwrap();
        assert_eq!(runtime.spawn(SinkActor), Err(SpawnError::CapacityExceeded));
    }

    #[test]
    fn timers_fire_on_concurrent_runtime() {
        let runtime = ConcurrentRuntime::default();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let token = TimerToken::next();
        runtime
            .spawn(TimerActor {
                token,
                seen: Arc::clone(&seen),
            })
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(1);
        while seen.lock().unwrap().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }

        assert_eq!(seen.lock().unwrap().as_slice(), &[token]);
    }

    #[test]
    fn blocking_cpu_completion_returns_to_mailbox() {
        let runtime = ConcurrentRuntime::default();
        let seen = Arc::new(Mutex::new(Vec::new()));

        runtime
            .spawn(BlockingActor {
                seen: Arc::clone(&seen),
                expected: None,
            })
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(1);
        while seen.lock().unwrap().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].1, PoolKind::BlockingCpu);
        assert_eq!(seen[0].2, 42);
    }

    #[test]
    fn ask_timeout_is_delivered_to_caller() {
        let runtime = ConcurrentRuntime::default();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let server = runtime.spawn(NeverReplyActor).unwrap();

        runtime
            .spawn(TimeoutClient {
                server,
                seen: Arc::clone(&seen),
                pending: None,
            })
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(1);
        while seen.lock().unwrap().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }

        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn observability_apis_work_on_concurrent_runtime() {
        let runtime = ConcurrentRuntime::new(SchedulerConfig {
            scheduler_count: 2,
            ..SchedulerConfig::default()
        });
        let root = runtime
            .spawn_with_options(
                SinkActor,
                SpawnOptions {
                    registered_name: Some("root".into()),
                    ..SpawnOptions::default()
                },
            )
            .unwrap();
        let parent = runtime
            .spawn_with_options(
                SinkActor,
                SpawnOptions {
                    registered_name: Some("parent".into()),
                    parent: Some(root),
                    ancestors: vec![root],
                    ..SpawnOptions::default()
                },
            )
            .unwrap();
        let child = runtime
            .spawn_with_options(
                SinkActor,
                SpawnOptions {
                    registered_name: Some("child".into()),
                    parent: Some(parent),
                    ancestors: vec![root, parent],
                    ..SpawnOptions::default()
                },
            )
            .unwrap();

        assert!(runtime.wait_for_idle(Some(Duration::from_secs(1))));

        let introspection = runtime.introspection();
        assert_eq!(introspection.actors.len(), 3);
        assert_eq!(introspection.actor_tree.roots, vec![root]);
        let root_node = introspection
            .actor_tree
            .nodes
            .iter()
            .find(|node| node.actor.id == root)
            .unwrap();
        assert_eq!(root_node.children, vec![parent]);
        let parent_node = introspection
            .actor_tree
            .nodes
            .iter()
            .find(|node| node.actor.id == parent)
            .unwrap();
        assert_eq!(parent_node.children, vec![child]);

        let metrics = runtime.metrics_snapshot();
        assert_eq!(metrics.live_actors, 3);
        assert_eq!(metrics.waiting_actors, 3);
        assert!(
            runtime
                .export_metrics_prometheus()
                .contains("lamport_runtime_live_actors 3")
        );

        let mut cursor = runtime.event_cursor();
        assert!(runtime.exit_actor(child, ExitReason::Error("boom".into())));
        assert!(runtime.wait_for_idle(Some(Duration::from_secs(1))));

        let delta = runtime.events_since(&mut cursor);
        assert!(delta.iter().any(|event| {
            matches!(
                &event.kind,
                RuntimeEventKind::Lifecycle(LifecycleEvent::Exit { actor, .. }) if *actor == child
            )
        }));
        assert!(delta.iter().any(
            |event| matches!(&event.kind, RuntimeEventKind::Crash(report) if report.actor == child)
        ));

        let crash = runtime
            .crash_reports()
            .into_iter()
            .find(|report| report.actor == child)
            .unwrap();
        assert_eq!(crash.parent_identity().unwrap().actor, parent);
        assert_eq!(
            crash
                .ancestor_identities()
                .iter()
                .map(|identity| identity.actor)
                .collect::<Vec<_>>(),
            vec![root, parent]
        );
        let rendered = crash.to_string();
        assert!(rendered.contains("parent:"));
        assert!(rendered.contains("ancestors:"));
    }
}
