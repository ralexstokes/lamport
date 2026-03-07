mod lifecycle;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::mpsc,
    time::{Duration, Instant},
};

use tokio::{runtime::Runtime as TokioRuntime, task::JoinHandle};

use crate::{
    actor::{Actor, ActorTurn},
    behaviour::{GenServer, GenServerActor, GenStatem, GenStatemActor, RuntimeInfo},
    context::{
        ActorContext, LifecycleContext, LinkError, MonitorError, PendingCall, ReceivedEnvelope,
        SendError, SpawnError, SpawnOptions, SupervisorContext, TaskHandle, TimerError,
    },
    control::{ControlError, ControlResult, StateSnapshot, TraceOptions},
    envelope::{DownMessage, Envelope, ExitSignal, Message, ReplyToken, TaskCompleted, TimerFired},
    internal::{
        ExitDisposition, ShutdownMode, ShutdownTracker, SupervisorRuntimeState, TurnEffects,
        actor_mailbox, classify_exit_signal, mailbox_overflow_reason, normalize_scheduler_config,
        panic_reason, shutdown_signal,
    },
    lifecycle::{CrashReport, LifecycleEvent, ShutdownPhase},
    mailbox::{Mailbox, MailboxFull, MailboxWatermark},
    observability::{
        ActorTree, EventCursor, RuntimeEvent, RuntimeEventKind, RuntimeIntrospection,
        RuntimeMetricsSnapshot, TraceEvent, TraceEventKind, build_actor_tree,
        build_metrics_snapshot, build_runtime_introspection, emit_tracing_event, events_since,
    },
    registry::{Registry, RegistryError},
    scheduler::{PoolKind, RunQueueSnapshot, SchedulerConfig, SchedulerMetrics},
    snapshot::{ActorSnapshot, SupervisorSnapshot},
    supervisor::{Supervisor, SupervisorActor},
    types::{
        ActorId, ActorIdentity, ActorMetrics, ActorStatus, ChildSpec, ExitReason, Ref, Shutdown,
        SupervisorFlags, TimerToken,
    },
};

const SYSTEM_ACTOR_ID: ActorId = ActorId::new(u64::MAX, 0);

#[derive(Debug)]
enum DriverEvent {
    TimerFired {
        target: ActorId,
        token: TimerToken,
    },
    ShutdownTimedOut {
        target: ActorId,
    },
    TaskCompleted {
        target: ActorId,
        task: TaskCompleted,
    },
    CallTimedOut {
        target: ActorId,
        reference: Ref,
    },
}

#[derive(Debug)]
struct ActorState {
    id: ActorId,
    mailbox: Mailbox,
    links: BTreeSet<ActorId>,
    monitors_in: BTreeMap<Ref, ActorId>,
    monitors_out: BTreeMap<Ref, ActorId>,
    trap_exit: bool,
    trace_options: TraceOptions,
    suspended: bool,
    status: ActorStatus,
    metrics: ActorMetrics,
    parent: Option<ActorId>,
    ancestors: Vec<ActorId>,
    registered_name: Option<String>,
    supervisor_child: Option<&'static str>,
    supervisor: Option<SupervisorRuntimeState>,
    initialized: bool,
    scheduled: bool,
}

impl ActorState {
    fn new(id: ActorId, options: SpawnOptions, config: &SchedulerConfig) -> Self {
        Self {
            id,
            mailbox: actor_mailbox(config, options.mailbox_capacity),
            links: BTreeSet::new(),
            monitors_in: BTreeMap::new(),
            monitors_out: BTreeMap::new(),
            trap_exit: options.trap_exit,
            trace_options: TraceOptions::default(),
            suspended: false,
            status: ActorStatus::Starting,
            metrics: ActorMetrics::default(),
            parent: options.parent,
            ancestors: options.ancestors,
            registered_name: options.registered_name,
            supervisor_child: options.supervisor_child,
            supervisor: None,
            initialized: false,
            scheduled: false,
        }
    }

    fn snapshot(&self, name: &'static str) -> ActorSnapshot {
        ActorSnapshot {
            id: self.id,
            name,
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
            suspended: self.suspended,
            status: self.status,
            metrics: self.metrics.clone(),
        }
    }
}

trait ActorCell: Send {
    fn init(&mut self, ctx: &mut LocalContext<'_>) -> Result<(), ExitReason>;
    fn select_envelope(&mut self, ctx: &mut LocalContext<'_>) -> Option<ReceivedEnvelope>;
    fn handle(&mut self, envelope: Envelope, ctx: &mut LocalContext<'_>) -> ActorTurn;
    fn state_version(&self) -> u64;
    fn inspect_state(&mut self, ctx: &mut LocalContext<'_>) -> Result<StateSnapshot, ControlError>;
    fn replace_state(
        &mut self,
        snapshot: StateSnapshot,
        ctx: &mut LocalContext<'_>,
    ) -> Result<(), ControlError>;
    fn code_change(
        &mut self,
        target_version: u64,
        ctx: &mut LocalContext<'_>,
    ) -> Result<(), ControlError>;
    fn terminate(&mut self, reason: ExitReason, ctx: &mut LocalContext<'_>);
}

impl<A: Actor> ActorCell for A {
    fn init(&mut self, ctx: &mut LocalContext<'_>) -> Result<(), ExitReason> {
        Actor::init(self, ctx)
    }

    fn select_envelope(&mut self, ctx: &mut LocalContext<'_>) -> Option<ReceivedEnvelope> {
        Actor::select_envelope(self, ctx)
    }

    fn handle(&mut self, envelope: Envelope, ctx: &mut LocalContext<'_>) -> ActorTurn {
        Actor::handle(self, envelope, ctx)
    }

    fn state_version(&self) -> u64 {
        Actor::state_version(self)
    }

    fn inspect_state(&mut self, ctx: &mut LocalContext<'_>) -> Result<StateSnapshot, ControlError> {
        Actor::inspect_state(self, ctx)
    }

    fn replace_state(
        &mut self,
        snapshot: StateSnapshot,
        ctx: &mut LocalContext<'_>,
    ) -> Result<(), ControlError> {
        Actor::replace_state(self, snapshot, ctx)
    }

    fn code_change(
        &mut self,
        target_version: u64,
        ctx: &mut LocalContext<'_>,
    ) -> Result<(), ControlError> {
        Actor::code_change(self, target_version, ctx)
    }

    fn terminate(&mut self, reason: ExitReason, ctx: &mut LocalContext<'_>) {
        Actor::terminate(self, reason, ctx);
    }
}

struct ActorEntry {
    name: &'static str,
    actor: Box<dyn ActorCell>,
    state: ActorState,
}

impl ActorEntry {
    fn new<A: Actor>(
        id: ActorId,
        actor: A,
        options: SpawnOptions,
        config: &SchedulerConfig,
    ) -> Self {
        Self {
            name: actor.name(),
            actor: Box::new(actor),
            state: ActorState::new(id, options, config),
        }
    }

    fn snapshot(&self) -> ActorSnapshot {
        self.state.snapshot(self.name)
    }
}

struct LocalContext<'a> {
    runtime: &'a mut LocalRuntime,
    current: &'a mut ActorState,
    current_name: &'static str,
    effects: TurnEffects,
}

impl<'a> LocalContext<'a> {
    fn new(
        runtime: &'a mut LocalRuntime,
        current: &'a mut ActorState,
        current_name: &'static str,
    ) -> Self {
        Self {
            runtime,
            current,
            current_name,
            effects: TurnEffects::default(),
        }
    }

    fn finish(self) -> TurnEffects {
        self.effects
    }

    fn send_to(&mut self, to: ActorId, envelope: Envelope) -> Result<(), SendError> {
        self.maybe_trace_send(to, &envelope);
        if to == self.current.id {
            if let Some(actor_id) = push_envelope(self.current, envelope)? {
                self.runtime.run_queue.push_back(actor_id);
            }
            return Ok(());
        }

        self.runtime.enqueue_actor_envelope(to, envelope)
    }

    fn maybe_trace_send(&mut self, to: ActorId, envelope: &Envelope) {
        if !self.current.trace_options.sends {
            return;
        }

        self.runtime.record_trace_event(TraceEvent {
            actor: self.current.id,
            actor_name: self.current_name,
            kind: TraceEventKind::Sent {
                to,
                envelope_kind: envelope.kind(),
            },
            mailbox_len: self
                .current
                .trace_options
                .mailbox_depth
                .then_some(self.current.mailbox.len()),
            scheduler_id: self.current.trace_options.scheduler.then_some(0),
        });
    }

    fn enqueue_self_runtime_envelope(&mut self, envelope: Envelope, label: &'static str) {
        match push_envelope(self.current, envelope) {
            Ok(Some(actor_id)) => self.runtime.run_queue.push_back(actor_id),
            Ok(None) | Err(SendError::NoProc(_)) => {}
            Err(SendError::MailboxFull { .. }) => {
                self.effects.exit_reason = Some(mailbox_overflow_reason(self.current.id, label));
            }
        }
    }

    fn handle_current_exit_signal(&mut self, signal: ExitSignal) {
        match classify_exit_signal(self.current.trap_exit, &signal) {
            ExitDisposition::KillNow => self.effects.exit_reason = Some(signal.reason),
            ExitDisposition::Ignore => {}
            ExitDisposition::Enqueue => {
                self.enqueue_self_runtime_envelope(Envelope::Exit(signal), "exit");
            }
        }
    }

    fn spawn_inner<A: Actor>(
        &mut self,
        actor: A,
        mut options: SpawnOptions,
    ) -> Result<ActorId, SpawnError> {
        if options.parent.is_none() {
            options.parent = Some(self.current.id);
        }

        if options.ancestors.is_empty() {
            options.ancestors = self.current.ancestors.clone();
            options.ancestors.push(self.current.id);
        }

        let id = self.runtime.allocate_id();
        let mut entry = ActorEntry::new(id, actor, options.clone(), &self.runtime.config);

        if options.link_to_parent {
            entry.state.links.insert(self.current.id);
        }
        self.runtime.insert_actor(entry)?;

        if options.link_to_parent {
            self.current.links.insert(id);
        }

        if let Some(child_id) = options.supervisor_child
            && let Some(supervisor) = self.current.supervisor.as_mut()
        {
            supervisor.set_running(child_id, id);
        }
        Ok(id)
    }

    fn link_inner(&mut self, other: ActorId) -> Result<(), LinkError> {
        if other == self.current.id {
            return Ok(());
        }

        let Some(target) = self.runtime.actor_state_mut(other) else {
            return Err(LinkError::NoProc(other));
        };

        if !self.current.links.insert(other) {
            return Err(LinkError::AlreadyLinked(other));
        }

        target.links.insert(self.current.id);
        Ok(())
    }

    fn unlink_inner(&mut self, other: ActorId) -> Result<(), LinkError> {
        if other == self.current.id {
            return Ok(());
        }

        let Some(target) = self.runtime.actor_state_mut(other) else {
            return Err(LinkError::NoProc(other));
        };

        if !self.current.links.remove(&other) {
            return Err(LinkError::NotLinked(other));
        }

        target.links.remove(&self.current.id);
        Ok(())
    }

    fn monitor_inner(&mut self, other: ActorId) -> Ref {
        let reference = Ref::next();

        if other == self.current.id {
            self.current.monitors_in.insert(reference, self.current.id);
            self.current.monitors_out.insert(reference, self.current.id);
            return reference;
        }

        match self.runtime.actor_state_mut(other) {
            Some(target) => {
                self.current.monitors_out.insert(reference, other);
                target.monitors_in.insert(reference, self.current.id);
            }
            None => {
                self.enqueue_self_runtime_envelope(
                    Envelope::Down(DownMessage {
                        reference,
                        actor: other,
                        reason: ExitReason::NoProc,
                    }),
                    "down",
                );
            }
        }

        reference
    }

    fn demonitor_inner(&mut self, reference: Ref) -> Result<(), MonitorError> {
        let Some(target) = self.current.monitors_out.remove(&reference) else {
            return Err(MonitorError::UnknownRef(reference));
        };

        if target == self.current.id {
            self.current.monitors_in.remove(&reference);
            return Ok(());
        }

        if let Some(target_state) = self.runtime.actor_state_mut(target) {
            target_state.monitors_in.remove(&reference);
        }

        Ok(())
    }

    fn spawn_blocking_task<F, R>(&mut self, pool: PoolKind, job: F) -> TaskHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        match pool {
            PoolKind::BlockingIo => self.runtime.metrics.blocking_io_jobs += 1,
            PoolKind::BlockingCpu => self.runtime.metrics.blocking_cpu_jobs += 1,
            PoolKind::Normal => {}
        }

        let id = Ref::next();
        let target = self.current.id;
        let sender = self.runtime.event_tx.clone();
        drop(self.runtime.tokio.spawn_blocking(move || {
            let result = job();
            let _ = sender.send(DriverEvent::TaskCompleted {
                target,
                task: TaskCompleted {
                    reference: id,
                    pool,
                    result: crate::Payload::new(result),
                },
            });
        }));
        TaskHandle::new(id, pool)
    }
}

impl ActorContext for LocalContext<'_> {
    fn actor_id(&self) -> ActorId {
        self.current.id
    }

    fn scheduler_id(&self) -> Option<usize> {
        Some(0)
    }

    fn spawn<A: Actor>(&mut self, actor: A, options: SpawnOptions) -> Result<ActorId, SpawnError> {
        self.spawn_inner(actor, options)
    }

    fn whereis(&self, name: &str) -> Option<ActorId> {
        self.runtime.resolve_name(name)
    }

    fn register_name(&mut self, name: String) -> Result<(), RegistryError> {
        self.runtime
            .registry
            .register(self.current.id, name.clone())?;
        self.current.registered_name = Some(name);
        Ok(())
    }

    fn unregister_name(&mut self) -> Option<String> {
        let removed = self.runtime.registry.unregister(self.current.id)?;
        self.current.registered_name = None;
        Some(removed)
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
        let reply_to = ReplyToken::new(self.current.id, reference);
        let watermark = self.current.mailbox.watermark();

        self.send_to(to, Envelope::request(reply_to, message))?;
        if let Some(timeout) = timeout {
            self.runtime
                .spawn_call_timeout(self.current.id, reference, timeout);
        }
        Ok(PendingCall::new(reply_to, watermark, timeout))
    }

    fn mailbox_watermark(&self) -> MailboxWatermark {
        self.current.mailbox.watermark()
    }

    fn receive_next(&mut self) -> Option<ReceivedEnvelope> {
        if self.effects.claimed_envelope {
            return None;
        }

        let envelope = take_next_envelope(self.current)?;
        self.effects.claimed_envelope = true;
        Some(ReceivedEnvelope::new(envelope))
    }

    fn receive_selective<F>(&mut self, predicate: F) -> Option<ReceivedEnvelope>
    where
        F: FnMut(&Envelope) -> bool,
    {
        self.receive_selective_after_inner(None, predicate)
    }

    fn receive_selective_after<F>(
        &mut self,
        watermark: MailboxWatermark,
        predicate: F,
    ) -> Option<ReceivedEnvelope>
    where
        F: FnMut(&Envelope) -> bool,
    {
        self.receive_selective_after_inner(Some(watermark), predicate)
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
        self.current.trap_exit = enabled;
    }

    fn schedule_after(&mut self, delay: Duration, token: TimerToken) -> Result<(), TimerError> {
        self.runtime.cancel_actor_timer(self.current.id, token);
        self.runtime.spawn_timer(self.current.id, token, delay);
        Ok(())
    }

    fn cancel_timer(&mut self, token: TimerToken) -> bool {
        self.runtime.cancel_actor_timer(self.current.id, token)
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
        let linked = self.current.links.contains(&actor);
        let reason = ExitReason::Shutdown;
        let result = self
            .runtime
            .request_shutdown(self.current.id, actor, policy);

        if result.is_ok() && linked && !self.runtime.contains(actor) {
            self.current.links.remove(&actor);
            self.handle_current_exit_signal(ExitSignal {
                from: actor,
                reason,
                linked: true,
            });
        }

        result
    }
}

impl LocalContext<'_> {
    fn receive_selective_after_inner<F>(
        &mut self,
        watermark: Option<MailboxWatermark>,
        predicate: F,
    ) -> Option<ReceivedEnvelope>
    where
        F: FnMut(&Envelope) -> bool,
    {
        if self.effects.claimed_envelope {
            return None;
        }

        let envelope = take_selective_envelope(self.current, watermark, predicate)?;
        self.effects.claimed_envelope = true;
        Some(ReceivedEnvelope::new(envelope))
    }
}

impl SupervisorContext for LocalContext<'_> {
    fn configure_supervisor(&mut self, flags: SupervisorFlags, child_specs: Vec<ChildSpec>) {
        self.current.supervisor = Some(SupervisorRuntimeState::new(flags, child_specs));
    }

    fn record_supervisor_restart(&mut self) -> bool {
        let allowed = self
            .current
            .supervisor
            .as_mut()
            .is_none_or(|supervisor| supervisor.intensity.record_restart(Instant::now()));

        if allowed {
            self.current.metrics.restarts += 1;
        }

        allowed
    }

    fn supervisor_child_started(&mut self, child_id: &'static str, actor: ActorId) {
        if let Some(supervisor) = self.current.supervisor.as_mut() {
            supervisor.set_running(child_id, actor);
        }
    }

    fn supervisor_child_exited(&mut self, child_id: &'static str, actor: ActorId) {
        if let Some(supervisor) = self.current.supervisor.as_mut() {
            supervisor.clear_running(child_id, actor);
        }
    }
}

impl LifecycleContext for LocalContext<'_> {
    fn emit_lifecycle_event(&mut self, event: LifecycleEvent) {
        self.runtime.record_lifecycle_event(event);
    }
}

/// Single-threaded runtime for the initial OTP-style execution model.
pub struct LocalRuntime {
    config: SchedulerConfig,
    tokio: TokioRuntime,
    event_tx: mpsc::Sender<DriverEvent>,
    event_rx: mpsc::Receiver<DriverEvent>,
    next_local_id: u64,
    live_actors: usize,
    actors: HashMap<u64, ActorEntry>,
    completed: HashMap<ActorId, ActorSnapshot>,
    registry: Registry,
    run_queue: VecDeque<ActorId>,
    timer_tasks: HashMap<(ActorId, TimerToken), JoinHandle<()>>,
    call_timeout_tasks: HashMap<(ActorId, Ref), JoinHandle<()>>,
    shutdown_tasks: HashMap<ActorId, ShutdownTracker>,
    lifecycle_events: Vec<LifecycleEvent>,
    crash_reports: Vec<CrashReport>,
    event_log: Vec<RuntimeEvent>,
    next_event_sequence: u64,
    metrics: SchedulerMetrics,
}

impl Default for LocalRuntime {
    fn default() -> Self {
        Self::new(SchedulerConfig {
            scheduler_count: 1,
            ..SchedulerConfig::default()
        })
    }
}

impl LocalRuntime {
    /// Creates a new runtime and returns any Tokio initialization error.
    pub fn try_new(config: SchedulerConfig) -> Result<Self, std::io::Error> {
        let mut config = normalize_scheduler_config(config);
        config.scheduler_count = 1;

        let tokio = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(
                config
                    .blocking_io_threads
                    .saturating_add(config.blocking_cpu_threads)
                    .max(1),
            )
            .enable_time()
            .thread_name("lamport-tokio")
            .build()?;
        let (event_tx, event_rx) = mpsc::channel();

        Ok(Self {
            config,
            tokio,
            event_tx,
            event_rx,
            next_local_id: 0,
            live_actors: 0,
            actors: HashMap::new(),
            completed: HashMap::new(),
            registry: Registry::new(),
            run_queue: VecDeque::new(),
            timer_tasks: HashMap::new(),
            call_timeout_tasks: HashMap::new(),
            shutdown_tasks: HashMap::new(),
            lifecycle_events: Vec::new(),
            crash_reports: Vec::new(),
            event_log: Vec::new(),
            next_event_sequence: 1,
            metrics: SchedulerMetrics::default(),
        })
    }

    /// Creates a new runtime with a single local scheduler and configurable pool sizes.
    pub fn new(config: SchedulerConfig) -> Self {
        Self::try_new(config).expect("failed to initialize Tokio runtime for lamport")
    }

    /// Boots an application root supervisor into an existing local runtime.
    pub fn boot_application<A: crate::application::Application>(
        &mut self,
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
    pub fn spawn<A: Actor>(&mut self, actor: A) -> Result<ActorId, SpawnError> {
        self.spawn_with_options(actor, SpawnOptions::default())
    }

    /// Spawns a new actor with explicit options.
    pub fn spawn_with_options<A: Actor>(
        &mut self,
        actor: A,
        options: SpawnOptions,
    ) -> Result<ActorId, SpawnError> {
        let id = self.allocate_id();
        self.insert_actor(ActorEntry::new(id, actor, options, &self.config))?;
        Ok(id)
    }

    /// Spawns a typed `GenServer` with default options.
    pub fn spawn_gen_server<G>(&mut self, server: G) -> Result<ActorId, SpawnError>
    where
        G: GenServer,
        G::Info: From<RuntimeInfo>,
        G::State: Send,
    {
        self.spawn_gen_server_with_options(server, SpawnOptions::default())
    }

    /// Spawns a typed `GenServer` with explicit options.
    pub fn spawn_gen_server_with_options<G>(
        &mut self,
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
    pub fn spawn_gen_statem<G>(&mut self, machine: G) -> Result<ActorId, SpawnError>
    where
        G: GenStatem,
        G::Info: From<RuntimeInfo>,
    {
        self.spawn_gen_statem_with_options(machine, SpawnOptions::default())
    }

    /// Spawns a typed `GenStatem` with explicit options.
    pub fn spawn_gen_statem_with_options<G>(
        &mut self,
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
    pub fn spawn_supervisor<S>(&mut self, supervisor: S) -> Result<ActorId, SpawnError>
    where
        S: Supervisor,
    {
        self.spawn_supervisor_with_options(supervisor, SpawnOptions::default())
    }

    /// Spawns a typed `Supervisor` with explicit options.
    pub fn spawn_supervisor_with_options<S>(
        &mut self,
        supervisor: S,
        options: SpawnOptions,
    ) -> Result<ActorId, SpawnError>
    where
        S: Supervisor,
    {
        self.spawn_with_options(SupervisorActor::new(supervisor), options)
    }

    /// Sends a typed message from outside the runtime.
    pub fn send<M: Message>(&mut self, to: ActorId, message: M) -> Result<(), SendError> {
        self.send_envelope(to, Envelope::user(message))
    }

    /// Sends a raw envelope from outside the runtime.
    pub fn send_envelope(&mut self, to: ActorId, envelope: Envelope) -> Result<(), SendError> {
        self.enqueue_actor_envelope(to, envelope)
    }

    /// Sends a reserved runtime control message from outside the runtime.
    pub fn send_system(
        &mut self,
        to: ActorId,
        message: crate::envelope::SystemMessage,
    ) -> Result<(), SendError> {
        self.send_envelope(to, Envelope::System(message))
    }

    /// Requests runtime-managed shutdown using the same path as supervisors.
    pub fn shutdown_actor(&mut self, actor: ActorId, policy: Shutdown) -> Result<(), SendError> {
        self.request_shutdown(SYSTEM_ACTOR_ID, actor, policy)
    }

    /// Retrieves a type-erased actor state snapshot through the reserved control lane.
    pub fn get_state(&mut self, actor: ActorId) -> ControlResult<StateSnapshot> {
        let (reply, rx) = mpsc::channel();
        self.send_system(actor, crate::envelope::SystemMessage::GetState { reply })
            .map_err(|error| map_send_error(actor, error, "GetState"))?;
        self.wait_for_control_reply(actor, "GetState", rx)
    }

    /// Replaces actor state through the reserved control lane.
    pub fn replace_state(&mut self, actor: ActorId, state: StateSnapshot) -> ControlResult<()> {
        let (reply, rx) = mpsc::channel();
        self.send_system(
            actor,
            crate::envelope::SystemMessage::ReplaceState { state, reply },
        )
        .map_err(|error| map_send_error(actor, error, "ReplaceState"))?;
        self.wait_for_control_reply(actor, "ReplaceState", rx)
    }

    /// Enables actor-scoped tracing through the reserved control lane.
    pub fn trace_actor(&mut self, actor: ActorId, options: TraceOptions) -> ControlResult<()> {
        let (reply, rx) = mpsc::channel();
        self.send_system(
            actor,
            crate::envelope::SystemMessage::TraceOn { options, reply },
        )
        .map_err(|error| map_send_error(actor, error, "TraceOn"))?;
        self.wait_for_control_reply(actor, "TraceOn", rx)
    }

    /// Disables actor-scoped tracing through the reserved control lane.
    pub fn untrace_actor(&mut self, actor: ActorId) -> ControlResult<()> {
        let (reply, rx) = mpsc::channel();
        self.send_system(actor, crate::envelope::SystemMessage::TraceOff { reply })
            .map_err(|error| map_send_error(actor, error, "TraceOff"))?;
        self.wait_for_control_reply(actor, "TraceOff", rx)
    }

    /// Runs an actor-defined code-change hook through the reserved control lane.
    pub fn code_change_actor(&mut self, actor: ActorId, target_version: u64) -> ControlResult<()> {
        let (reply, rx) = mpsc::channel();
        self.send_system(
            actor,
            crate::envelope::SystemMessage::CodeChange {
                target_version,
                reply,
            },
        )
        .map_err(|error| map_send_error(actor, error, "CodeChange"))?;
        self.wait_for_control_reply(actor, "CodeChange", rx)
    }

    /// Suspends normal user-envelope execution for the actor.
    pub fn suspend_actor(&mut self, actor: ActorId) -> Result<(), SendError> {
        self.send_system(actor, crate::envelope::SystemMessage::Suspend)
    }

    /// Resumes normal user-envelope execution for the actor.
    pub fn resume_actor(&mut self, actor: ActorId) -> Result<(), SendError> {
        self.send_system(actor, crate::envelope::SystemMessage::Resume)
    }

    /// Forces an actor to exit immediately.
    pub fn exit_actor(&mut self, actor: ActorId, reason: ExitReason) -> bool {
        self.force_exit(actor, reason)
    }

    /// Executes at most one actor turn.
    pub fn run_once(&mut self) -> bool {
        let made_progress = self.drain_external_events();

        while let Some(actor_id) = self.run_queue.pop_front() {
            let Some(mut entry) = self.take_actor(actor_id) else {
                continue;
            };

            entry.state.scheduled = false;
            self.drive_actor(entry);
            return true;
        }

        if !made_progress {
            self.metrics.idle_turns += 1;
        }

        made_progress
    }

    /// Runs until no actor is runnable and no timers are immediately due.
    pub fn run_until_idle(&mut self) -> usize {
        let mut steps = 0;

        while self.run_once() {
            steps += 1;
        }

        steps
    }

    /// Runs one actor turn, waiting for the next external event when needed.
    pub fn block_on_next(&mut self, timeout: Option<Duration>) -> bool {
        if self.run_once() {
            return true;
        }

        if !self.wait_for_external_event(timeout) {
            return false;
        }

        self.run_once()
    }

    /// Returns the current or completed snapshot for an actor.
    pub fn actor_snapshot(&self, actor: ActorId) -> Option<ActorSnapshot> {
        self.actors
            .get(&actor.local_id)
            .and_then(|entry| (entry.state.id == actor).then(|| entry.snapshot()))
            .or_else(|| self.completed.get(&actor).cloned())
    }

    /// Returns the tracked supervisor state for a live supervisor actor.
    pub fn supervisor_snapshot(&mut self, actor: ActorId) -> Option<SupervisorSnapshot> {
        self.actors
            .get_mut(&actor.local_id)
            .and_then(|entry| (entry.state.id == actor).then_some(&mut entry.state))
            .and_then(|state| {
                state
                    .supervisor
                    .as_mut()
                    .map(|supervisor| supervisor.snapshot(actor))
            })
    }

    /// Returns the accumulated runtime lifecycle events.
    pub fn lifecycle_events(&self) -> Vec<LifecycleEvent> {
        self.lifecycle_events.clone()
    }

    /// Returns the accumulated abnormal crash reports.
    pub fn crash_reports(&self) -> Vec<CrashReport> {
        self.crash_reports.clone()
    }

    /// Returns retained structured runtime events for tracing and debugging.
    pub fn event_log(&self) -> Vec<RuntimeEvent> {
        self.event_log.clone()
    }

    /// Returns a cursor positioned after the current retained event log.
    pub fn event_cursor(&self) -> EventCursor {
        EventCursor::new(self.next_event_sequence)
    }

    /// Returns events emitted after the cursor and advances it.
    pub fn events_since(&self, cursor: &mut EventCursor) -> Vec<RuntimeEvent> {
        events_since(&self.event_log, cursor)
    }

    /// Returns snapshots for all live actors.
    pub fn actor_snapshots(&self) -> Vec<ActorSnapshot> {
        let mut snapshots: Vec<_> = self.actors.values().map(ActorEntry::snapshot).collect();
        snapshots.sort_by_key(|snapshot| snapshot.id);
        snapshots
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
            self.completed.len(),
            self.metrics(),
            vec![self.run_queue_snapshot()],
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
            self.completed.len(),
            self.metrics(),
            vec![self.run_queue_snapshot()],
        )
    }

    /// Looks up a registered actor by name.
    pub fn resolve_name(&self, name: &str) -> Option<ActorId> {
        self.registry.resolve(name)
    }

    /// Registers a live actor under the provided name.
    pub fn register_name(&mut self, actor: ActorId, name: String) -> Result<(), RegistryError> {
        if !self.contains(actor) {
            return Err(RegistryError::NoProc(actor));
        }

        self.registry.register(actor, name.clone())?;
        if let Some(state) = self.actor_state_mut(actor) {
            state.registered_name = Some(name);
        }
        Ok(())
    }

    /// Removes the current registered name for a live actor.
    pub fn unregister_name(&mut self, actor: ActorId) -> Option<String> {
        let removed = self.registry.unregister(actor)?;
        if let Some(state) = self.actor_state_mut(actor) {
            state.registered_name = None;
        }
        Some(removed)
    }

    /// Returns whether the actor id refers to a live actor.
    pub fn contains(&self, actor: ActorId) -> bool {
        self.actors
            .get(&actor.local_id)
            .is_some_and(|entry| entry.state.id == actor)
    }

    fn wait_for_control_reply<T>(
        &mut self,
        actor: ActorId,
        operation: &'static str,
        rx: mpsc::Receiver<ControlResult<T>>,
    ) -> ControlResult<T> {
        loop {
            match rx.try_recv() {
                Ok(result) => return result,
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(ControlError::rejected(
                        operation,
                        "control response channel closed",
                    ));
                }
                Err(mpsc::TryRecvError::Empty) => {
                    if !self.run_once() {
                        return if self.contains(actor) {
                            Err(ControlError::rejected(
                                operation,
                                "control request did not complete",
                            ))
                        } else {
                            Err(ControlError::NoProc(actor))
                        };
                    }
                }
            }
        }
    }

    /// Returns a single-scheduler snapshot for observer-style UIs.
    pub fn run_queue_snapshot(&self) -> RunQueueSnapshot {
        let mut runnable = 0;
        let mut waiting = 0;

        for entry in self.actors.values() {
            match entry.state.status {
                ActorStatus::Starting | ActorStatus::Runnable | ActorStatus::Running => {
                    runnable += 1;
                }
                ActorStatus::Waiting => waiting += 1,
                ActorStatus::Exiting | ActorStatus::Dead => {}
            }
        }

        RunQueueSnapshot {
            scheduler_id: 0,
            runnable,
            waiting,
            injected: 0,
            stolen: 0,
        }
    }

    /// Returns aggregate runtime metrics.
    pub fn metrics(&self) -> SchedulerMetrics {
        self.metrics
    }

    fn record_runtime_event(&mut self, kind: RuntimeEventKind) {
        let record = RuntimeEvent {
            sequence: self.next_event_sequence,
            emitted_at: std::time::SystemTime::now(),
            kind,
        };
        self.next_event_sequence += 1;
        emit_tracing_event(&record);
        self.event_log.push(record);
    }

    fn record_lifecycle_event(&mut self, event: LifecycleEvent) {
        self.lifecycle_events.push(event.clone());
        self.record_runtime_event(RuntimeEventKind::Lifecycle(event));
    }

    fn record_crash_report(&mut self, report: CrashReport) {
        self.crash_reports.push(report.clone());
        self.record_runtime_event(RuntimeEventKind::Crash(report));
    }

    fn record_trace_event(&mut self, event: TraceEvent) {
        self.record_runtime_event(RuntimeEventKind::Trace(event));
    }

    fn actor_identity(&self, actor: ActorId) -> Option<ActorIdentity> {
        self.actors
            .get(&actor.local_id)
            .and_then(|entry| (entry.state.id == actor).then(|| entry.snapshot()))
            .or_else(|| self.completed.get(&actor).cloned())
            .map(|snapshot| {
                ActorIdentity::new(snapshot.id, snapshot.name, snapshot.registered_name.clone())
            })
    }

    fn allocate_id(&mut self) -> ActorId {
        let id = ActorId::new(self.next_local_id, 0);
        self.next_local_id += 1;
        id
    }

    fn enqueue_actor_envelope(
        &mut self,
        actor: ActorId,
        envelope: Envelope,
    ) -> Result<(), SendError> {
        if let Envelope::Reply { reference, .. } = &envelope {
            self.cancel_call_timeout(actor, *reference);
        }

        let scheduled = {
            let Some(target) = self.actor_state_mut(actor) else {
                return Err(SendError::NoProc(actor));
            };

            push_envelope(target, envelope)?
        };

        if let Some(actor_id) = scheduled {
            self.run_queue.push_back(actor_id);
        }

        Ok(())
    }

    fn enqueue_runtime_envelope(
        &mut self,
        actor: ActorId,
        envelope: Envelope,
        label: &'static str,
    ) -> bool {
        match self.enqueue_actor_envelope(actor, envelope) {
            Ok(()) => true,
            Err(SendError::MailboxFull { .. }) => {
                self.force_exit(actor, mailbox_overflow_reason(actor, label))
            }
            Err(SendError::NoProc(_)) => false,
        }
    }

    fn insert_actor(&mut self, mut entry: ActorEntry) -> Result<(), SpawnError> {
        if self.live_actors >= self.config.max_actors {
            return Err(SpawnError::CapacityExceeded);
        }

        let actor_id = entry.state.id;
        let parent = entry.state.parent;
        let registered_name = entry.state.registered_name.clone();
        let supervisor_child = entry.state.supervisor_child;
        let name = entry.name;

        if let Some(name) = entry.state.registered_name.clone() {
            self.registry
                .register(entry.state.id, name)
                .map_err(SpawnError::Registry)?;
        }

        entry.state.status = ActorStatus::Starting;
        schedule_actor(&mut self.run_queue, &mut entry.state);
        self.actors.insert(entry.state.id.local_id, entry);
        self.live_actors += 1;

        if let (Some(parent), Some(child_id)) = (parent, supervisor_child)
            && let Some(parent_state) = self.actor_state_mut(parent)
            && let Some(supervisor) = parent_state.supervisor.as_mut()
        {
            supervisor.set_running(child_id, actor_id);
        }

        self.record_lifecycle_event(LifecycleEvent::Spawn {
            actor: actor_id,
            name,
            registered_name,
            parent,
            supervisor_child,
        });
        Ok(())
    }

    fn take_actor(&mut self, id: ActorId) -> Option<ActorEntry> {
        let entry = self.actors.remove(&id.local_id)?;

        if entry.state.id == id {
            Some(entry)
        } else {
            self.actors.insert(entry.state.id.local_id, entry);
            None
        }
    }

    fn actor_state_mut(&mut self, actor: ActorId) -> Option<&mut ActorState> {
        self.actors
            .get_mut(&actor.local_id)
            .and_then(|entry| (entry.state.id == actor).then_some(&mut entry.state))
    }

    fn request_shutdown(
        &mut self,
        requester: ActorId,
        actor: ActorId,
        policy: Shutdown,
    ) -> Result<(), SendError> {
        if !self.contains(actor) {
            return Err(SendError::NoProc(actor));
        }

        self.cancel_shutdown(actor);
        self.record_lifecycle_event(LifecycleEvent::Shutdown {
            requester,
            actor,
            policy: policy.clone(),
            phase: ShutdownPhase::Requested,
            reason: None,
        });

        match ShutdownMode::from_policy(&policy) {
            ShutdownMode::ForceKill => {
                self.shutdown_tasks
                    .insert(actor, ShutdownTracker::new(requester, policy, None));
                self.force_exit(actor, ExitReason::Kill);
            }
            ShutdownMode::Linked {
                timeout: Some(delay),
            } => {
                let task = self.spawn_shutdown_timer(actor, delay);
                self.begin_linked_shutdown(requester, actor, policy, Some(task));
            }
            ShutdownMode::Linked { timeout: None } => {
                self.begin_linked_shutdown(requester, actor, policy, None);
            }
        }

        Ok(())
    }

    fn begin_linked_shutdown(
        &mut self,
        requester: ActorId,
        actor: ActorId,
        policy: Shutdown,
        task: Option<JoinHandle<()>>,
    ) {
        self.shutdown_tasks
            .insert(actor, ShutdownTracker::new(requester, policy, task));
        self.propagate_link_exit(actor, shutdown_signal(requester));
    }

    fn cancel_shutdown(&mut self, actor: ActorId) {
        if let Some(mut tracker) = self.shutdown_tasks.remove(&actor)
            && let Some(task) = tracker.task.take()
        {
            task.abort();
        }
    }

    fn drive_actor(&mut self, mut entry: ActorEntry) {
        if !entry.state.initialized {
            self.run_init(entry);
            return;
        }

        let (selection, turn_result, effects) = {
            let actor_id = entry.state.id;
            let actor_name = entry.name;
            let mut ctx = LocalContext::new(self, &mut entry.state, actor_name);
            let selected = catch_unwind(AssertUnwindSafe(|| entry.actor.select_envelope(&mut ctx)));

            match selected {
                Ok(Some(selected)) => {
                    let envelope = selected.into_envelope();
                    apply_system_message_effects(ctx.current, &envelope);

                    ctx.current.metrics.mailbox_len = ctx.current.mailbox.len();
                    ctx.current.status = ActorStatus::Running;
                    ctx.current.metrics.scheduler_id = Some(0);
                    ctx.current.metrics.turns_run += 1;
                    ctx.runtime.metrics.normal_turns += 1;
                    maybe_trace_receive(ctx.runtime, ctx.current, actor_name, &envelope);

                    let envelope_kind = envelope.kind();
                    let turn_span = tracing::trace_span!(
                        "lamport.actor.turn",
                        actor_id = %actor_id,
                        actor_name = actor_name,
                        scheduler_id = 0usize,
                        envelope_kind = ?envelope_kind
                    );
                    let _turn_guard = turn_span.enter();
                    let result = catch_unwind(AssertUnwindSafe(|| {
                        if let Envelope::System(message) = envelope {
                            match handle_reserved_system_message(
                                entry.actor.as_mut(),
                                actor_name,
                                message,
                                &mut ctx,
                            ) {
                                ReservedSystemResult::Handled(turn) => turn,
                                ReservedSystemResult::Forward(message) => {
                                    entry.actor.handle(Envelope::System(message), &mut ctx)
                                }
                            }
                        } else {
                            entry.actor.handle(envelope, &mut ctx)
                        }
                    }));
                    (Some(()), result, ctx.finish())
                }
                Ok(None) => (None, Ok(ActorTurn::Continue), ctx.finish()),
                Err(panic) => (Some(()), Err(panic), ctx.finish()),
            }
        };

        if selection.is_none() {
            if let Some(reason) = effects.exit_reason {
                self.finish_actor(entry, reason);
                return;
            }

            park_actor(&mut entry.state);
            self.actors.insert(entry.state.id.local_id, entry);
            return;
        }

        match turn_result {
            Ok(turn) => {
                let exit_reason = effects.exit_reason.or(match turn {
                    ActorTurn::Stop(reason) => Some(reason),
                    ActorTurn::Continue | ActorTurn::Yield => None,
                });

                if let Some(reason) = exit_reason {
                    self.finish_actor(entry, reason);
                    return;
                }

                if effects.yielded {
                    entry.state.status = ActorStatus::Runnable;
                }

                self.return_actor(entry);
            }
            Err(panic) => {
                let actor_id = entry.state.id;
                let name = entry.name;
                self.finish_actor(entry, panic_reason(actor_id, name, panic));
            }
        }
    }

    fn spawn_timer(&mut self, target: ActorId, token: TimerToken, delay: Duration) {
        let sender = self.event_tx.clone();
        let handle = self.tokio.spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = sender.send(DriverEvent::TimerFired { target, token });
        });

        self.timer_tasks.insert((target, token), handle);
    }

    fn spawn_call_timeout(&mut self, target: ActorId, reference: Ref, delay: Duration) {
        self.cancel_call_timeout(target, reference);

        let sender = self.event_tx.clone();
        let handle = self.tokio.spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = sender.send(DriverEvent::CallTimedOut { target, reference });
        });

        self.call_timeout_tasks.insert((target, reference), handle);
    }

    fn spawn_shutdown_timer(&mut self, target: ActorId, delay: Duration) -> JoinHandle<()> {
        let sender = self.event_tx.clone();
        self.tokio.spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = sender.send(DriverEvent::ShutdownTimedOut { target });
        })
    }

    fn drain_external_events(&mut self) -> bool {
        let mut fired_any = false;

        while let Ok(event) = self.event_rx.try_recv() {
            fired_any |= self.ingest_event(event);
        }

        fired_any
    }

    fn wait_for_external_event(&mut self, timeout: Option<Duration>) -> bool {
        let received = match timeout {
            Some(duration) => self.event_rx.recv_timeout(duration).ok(),
            None => self.event_rx.recv().ok(),
        };

        let Some(event) = received else {
            return false;
        };

        let mut handled = self.ingest_event(event);
        handled |= self.drain_external_events();
        handled
    }

    fn ingest_event(&mut self, event: DriverEvent) -> bool {
        match event {
            DriverEvent::TimerFired { target, token } => {
                if self.timer_tasks.remove(&(target, token)).is_none() {
                    return false;
                }

                self.enqueue_runtime_envelope(
                    target,
                    Envelope::Timer(TimerFired { token }),
                    "timer",
                )
            }
            DriverEvent::TaskCompleted { target, task } => {
                self.enqueue_runtime_envelope(target, Envelope::Task(task), "task")
            }
            DriverEvent::CallTimedOut { target, reference } => {
                if self
                    .call_timeout_tasks
                    .remove(&(target, reference))
                    .is_none()
                {
                    return false;
                }

                self.enqueue_runtime_envelope(
                    target,
                    Envelope::call_timeout(reference),
                    "call timeout",
                )
            }
            DriverEvent::ShutdownTimedOut { target } => {
                let Some(tracker) = self.shutdown_tasks.get(&target) else {
                    return false;
                };

                self.record_lifecycle_event(LifecycleEvent::Shutdown {
                    requester: tracker.requester,
                    actor: target,
                    policy: tracker.policy.clone(),
                    phase: ShutdownPhase::TimedOut,
                    reason: Some(ExitReason::Kill),
                });

                self.force_exit(target, ExitReason::Kill)
            }
        }
    }

    fn cancel_actor_timer(&mut self, actor: ActorId, token: TimerToken) -> bool {
        let mut cancelled = false;
        if let Some(handle) = self.timer_tasks.remove(&(actor, token)) {
            handle.abort();
            cancelled = true;
        }

        if let Some(entry) = self
            .actors
            .get_mut(&actor.local_id)
            .and_then(|entry| (entry.state.id == actor).then_some(entry))
        {
            let removed = entry
                .state
                .mailbox
                .remove_matching(|envelope| matches!(
                    envelope,
                    Envelope::Timer(TimerFired { token: queued_token }) if *queued_token == token
                ))
                .is_some();
            if removed {
                entry.state.metrics.mailbox_len = entry.state.mailbox.len();
            }
            cancelled |= removed;
        }

        cancelled
    }

    fn cancel_all_timers(&mut self, actor: ActorId) {
        let tokens: Vec<_> = self
            .timer_tasks
            .keys()
            .filter_map(|(target, token)| (*target == actor).then_some(*token))
            .collect();

        for token in tokens {
            self.cancel_actor_timer(actor, token);
        }
    }

    fn cancel_call_timeout(&mut self, actor: ActorId, reference: Ref) -> bool {
        let Some(handle) = self.call_timeout_tasks.remove(&(actor, reference)) else {
            return false;
        };

        handle.abort();
        true
    }

    fn cancel_all_call_timeouts(&mut self, actor: ActorId) {
        let references: Vec<_> = self
            .call_timeout_tasks
            .keys()
            .filter_map(|(target, reference)| (*target == actor).then_some(*reference))
            .collect();

        for reference in references {
            self.cancel_call_timeout(actor, reference);
        }
    }
}

fn schedule_actor(run_queue: &mut VecDeque<ActorId>, state: &mut ActorState) {
    if let Some(actor_id) = mark_scheduled(state) {
        run_queue.push_back(actor_id);
    }
}

enum ReservedSystemResult {
    Handled(ActorTurn),
    Forward(crate::envelope::SystemMessage),
}

fn handle_reserved_system_message(
    actor: &mut dyn ActorCell,
    actor_name: &'static str,
    message: crate::envelope::SystemMessage,
    ctx: &mut LocalContext<'_>,
) -> ReservedSystemResult {
    match message {
        crate::envelope::SystemMessage::GetState { reply } => {
            match actor.inspect_state(ctx) {
                Ok(snapshot) => {
                    let version = snapshot.version;
                    let sent = reply.send(Ok(snapshot)).is_ok();
                    if sent && ctx.current.trace_options.is_enabled() {
                        ctx.runtime.record_trace_event(build_trace_event(
                            ctx.current,
                            actor_name,
                            TraceEventKind::StateInspected { version },
                        ));
                    }
                }
                Err(error) => {
                    let _ = reply.send(Err(error));
                }
            }
            ReservedSystemResult::Handled(ActorTurn::Continue)
        }
        crate::envelope::SystemMessage::ReplaceState {
            state: snapshot,
            reply,
        } => {
            let result = validate_snapshot_version(actor, &snapshot)
                .and_then(|()| actor.replace_state(snapshot, ctx));
            if result.is_ok() && ctx.current.trace_options.is_enabled() {
                ctx.runtime.record_trace_event(build_trace_event(
                    ctx.current,
                    actor_name,
                    TraceEventKind::StateReplaced {
                        version: actor.state_version(),
                    },
                ));
            }
            let _ = reply.send(result);
            ReservedSystemResult::Handled(ActorTurn::Continue)
        }
        crate::envelope::SystemMessage::TraceOn { options, reply } => {
            ctx.current.trace_options = options;
            ctx.runtime.record_trace_event(build_trace_event(
                ctx.current,
                actor_name,
                TraceEventKind::TraceEnabled { options },
            ));
            let _ = reply.send(Ok(()));
            ReservedSystemResult::Handled(ActorTurn::Continue)
        }
        crate::envelope::SystemMessage::TraceOff { reply } => {
            if ctx.current.trace_options.is_enabled() {
                ctx.runtime.record_trace_event(build_trace_event(
                    ctx.current,
                    actor_name,
                    TraceEventKind::TraceDisabled,
                ));
            }
            ctx.current.trace_options = TraceOptions::default();
            let _ = reply.send(Ok(()));
            ReservedSystemResult::Handled(ActorTurn::Continue)
        }
        crate::envelope::SystemMessage::CodeChange {
            target_version,
            reply,
        } => {
            let from_version = actor.state_version();
            let result = actor.code_change(target_version, ctx);
            if result.is_ok() && ctx.current.trace_options.is_enabled() {
                ctx.runtime.record_trace_event(build_trace_event(
                    ctx.current,
                    actor_name,
                    TraceEventKind::CodeChanged {
                        from_version,
                        to_version: target_version,
                    },
                ));
            }
            let _ = reply.send(result);
            ReservedSystemResult::Handled(ActorTurn::Continue)
        }
        other => ReservedSystemResult::Forward(other),
    }
}

fn validate_snapshot_version(
    actor: &dyn ActorCell,
    snapshot: &StateSnapshot,
) -> Result<(), ControlError> {
    let current = actor.state_version();
    if current == snapshot.version {
        Ok(())
    } else {
        Err(ControlError::VersionMismatch {
            current,
            requested: snapshot.version,
        })
    }
}

fn build_trace_event(
    state: &ActorState,
    actor_name: &'static str,
    kind: TraceEventKind,
) -> TraceEvent {
    TraceEvent {
        actor: state.id,
        actor_name,
        kind,
        mailbox_len: state
            .trace_options
            .mailbox_depth
            .then_some(state.mailbox.len()),
        scheduler_id: state.trace_options.scheduler.then_some(0),
    }
}

fn maybe_trace_receive(
    runtime: &mut LocalRuntime,
    state: &ActorState,
    actor_name: &'static str,
    envelope: &Envelope,
) {
    if !state.trace_options.receives {
        return;
    }

    runtime.record_trace_event(build_trace_event(
        state,
        actor_name,
        TraceEventKind::Received {
            envelope_kind: envelope.kind(),
        },
    ));
}

fn push_envelope(state: &mut ActorState, envelope: Envelope) -> Result<Option<ActorId>, SendError> {
    let MailboxFull { capacity } = match state.mailbox.try_push(envelope) {
        Ok(()) => {
            state.metrics.mailbox_len = state.mailbox.len();

            if !matches!(state.status, ActorStatus::Dead | ActorStatus::Exiting) {
                return Ok(mark_scheduled(state));
            }

            return Ok(None);
        }
        Err(error) => error,
    };

    state.metrics.mailbox_len = state.mailbox.len();
    Err(SendError::MailboxFull {
        actor: state.id,
        capacity,
    })
}

fn take_next_envelope(state: &mut ActorState) -> Option<Envelope> {
    if state.suspended {
        return state
            .mailbox
            .selective_receive(Envelope::can_run_while_suspended);
    }

    state
        .mailbox
        .selective_receive(Envelope::is_system_message)
        .or_else(|| state.mailbox.pop_front())
}

fn take_selective_envelope<F>(
    state: &mut ActorState,
    watermark: Option<MailboxWatermark>,
    mut predicate: F,
) -> Option<Envelope>
where
    F: FnMut(&Envelope) -> bool,
{
    if state.suspended {
        return state
            .mailbox
            .selective_receive(Envelope::can_run_while_suspended);
    }

    if let Some(envelope) = state
        .mailbox
        .selective_receive(Envelope::bypasses_user_selective_receive)
    {
        return Some(envelope);
    }

    match watermark {
        Some(watermark) => state
            .mailbox
            .selective_receive_after(watermark, |envelope| {
                !envelope.bypasses_user_selective_receive() && predicate(envelope)
            }),
        None => state.mailbox.selective_receive(|envelope| {
            !envelope.bypasses_user_selective_receive() && predicate(envelope)
        }),
    }
}

fn apply_system_message_effects(state: &mut ActorState, envelope: &Envelope) {
    let Envelope::System(message) = envelope else {
        return;
    };

    match message {
        crate::envelope::SystemMessage::Suspend => state.suspended = true,
        crate::envelope::SystemMessage::Resume => state.suspended = false,
        crate::envelope::SystemMessage::GetState { .. }
        | crate::envelope::SystemMessage::ReplaceState { .. }
        | crate::envelope::SystemMessage::TraceOn { .. }
        | crate::envelope::SystemMessage::TraceOff { .. }
        | crate::envelope::SystemMessage::Shutdown
        | crate::envelope::SystemMessage::CodeChange { .. } => {}
    }
}

fn mark_scheduled(state: &mut ActorState) -> Option<ActorId> {
    if state.scheduled || matches!(state.status, ActorStatus::Dead | ActorStatus::Exiting) {
        return None;
    }

    state.scheduled = true;
    if !matches!(state.status, ActorStatus::Starting) {
        state.status = ActorStatus::Runnable;
    }

    Some(state.id)
}

fn park_actor(state: &mut ActorState) {
    state.status = ActorStatus::Waiting;
    state.metrics.scheduler_id = None;
    state.metrics.mailbox_len = state.mailbox.len();
    state.scheduled = false;
}

fn map_send_error(actor: ActorId, error: SendError, operation: &'static str) -> ControlError {
    match error {
        SendError::NoProc(_) => ControlError::NoProc(actor),
        SendError::MailboxFull { .. } => {
            ControlError::rejected(operation, "control mailbox delivery was backpressured")
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
            mpsc as std_mpsc,
        },
        time::{Duration, Instant},
    };

    use crate::{
        Actor, ActorTurn, Context, ControlError, Envelope, ExitReason, LifecycleEvent,
        LocalRuntime, PoolKind, ReceivedEnvelope, Ref, RegistryError, SchedulerConfig, SendError,
        Shutdown, SpawnError, SpawnOptions, StateSnapshot, SystemMessage, TimerToken, TraceOptions,
        envelope::{DownMessage, ExitSignal},
        observability::{EventCursor, RuntimeEventKind, TraceEventKind},
    };

    #[derive(Clone, PartialEq, Eq)]
    enum Control {
        Crash,
    }

    struct CrashActor;

    impl Actor for CrashActor {
        fn handle<C: Context>(&mut self, envelope: Envelope, _ctx: &mut C) -> ActorTurn {
            match envelope {
                Envelope::User(payload)
                    if payload.downcast_ref::<Control>() == Some(&Control::Crash) =>
                {
                    ActorTurn::Stop(ExitReason::Error("boom".into()))
                }
                _ => ActorTurn::Continue,
            }
        }
    }

    struct LinkedActor {
        target: crate::ActorId,
    }

    impl Actor for LinkedActor {
        fn init<C: Context>(&mut self, ctx: &mut C) -> Result<(), ExitReason> {
            ctx.link(self.target)
                .map_err(|error| ExitReason::Error(format!("link failed: {error:?}")))?;
            Ok(())
        }

        fn handle<C: Context>(&mut self, _envelope: Envelope, _ctx: &mut C) -> ActorTurn {
            ActorTurn::Continue
        }
    }

    struct TrapExitActor {
        target: crate::ActorId,
        seen: Arc<Mutex<Vec<ExitReason>>>,
    }

    impl Actor for TrapExitActor {
        fn init<C: Context>(&mut self, ctx: &mut C) -> Result<(), ExitReason> {
            ctx.link(self.target)
                .map_err(|error| ExitReason::Error(format!("link failed: {error:?}")))?;
            Ok(())
        }

        fn handle<C: Context>(&mut self, envelope: Envelope, _ctx: &mut C) -> ActorTurn {
            if let Envelope::Exit(signal) = envelope {
                self.seen.lock().unwrap().push(signal.reason);
            }

            ActorTurn::Continue
        }
    }

    struct MonitorMissingActor {
        missing: crate::ActorId,
        seen: Arc<Mutex<Vec<DownMessage>>>,
    }

    impl Actor for MonitorMissingActor {
        fn init<C: Context>(&mut self, ctx: &mut C) -> Result<(), ExitReason> {
            ctx.monitor(self.missing)
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

    struct MonitoringActor {
        target: crate::ActorId,
    }

    impl Actor for MonitoringActor {
        fn init<C: Context>(&mut self, ctx: &mut C) -> Result<(), ExitReason> {
            ctx.monitor(self.target)
                .map_err(|error| ExitReason::Error(format!("monitor failed: {error:?}")))?;
            Ok(())
        }

        fn handle<C: Context>(&mut self, _envelope: Envelope, _ctx: &mut C) -> ActorTurn {
            ActorTurn::Continue
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
        done: Arc<AtomicBool>,
        receiver: Option<std_mpsc::Receiver<()>>,
    }

    impl Actor for BlockingActor {
        fn init<C: Context>(&mut self, ctx: &mut C) -> Result<(), ExitReason> {
            let done = Arc::clone(&self.done);
            let receiver = self
                .receiver
                .take()
                .expect("blocking actor receiver should exist in init");

            let _handle = ctx.spawn_blocking_cpu(move || {
                let _ = receiver.recv_timeout(Duration::from_millis(200));
                done.store(true, Ordering::SeqCst);
            });

            Ok(())
        }

        fn handle<C: Context>(&mut self, _envelope: Envelope, _ctx: &mut C) -> ActorTurn {
            ActorTurn::Continue
        }
    }

    struct SinkActor;

    impl Actor for SinkActor {
        fn handle<C: Context>(&mut self, _envelope: Envelope, _ctx: &mut C) -> ActorTurn {
            ActorTurn::Continue
        }
    }

    struct TaskCompletionActor {
        seen: Arc<Mutex<Vec<(Ref, PoolKind, i32)>>>,
        expected: Option<Ref>,
    }

    impl Actor for TaskCompletionActor {
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

    struct ReplyActor;

    impl Actor for ReplyActor {
        fn handle<C: Context>(&mut self, envelope: Envelope, ctx: &mut C) -> ActorTurn {
            if let Envelope::Request { token, message } = envelope {
                let value = message.downcast::<u32>().ok().unwrap();
                let _ = ctx.reply(token, value + 1);
            }

            ActorTurn::Continue
        }
    }

    struct SelectiveReplyActor {
        server: crate::ActorId,
        seen: Arc<Mutex<Vec<String>>>,
        pending: Option<crate::PendingCall>,
    }

    impl Actor for SelectiveReplyActor {
        fn init<C: Context>(&mut self, ctx: &mut C) -> Result<(), ExitReason> {
            self.pending = Some(
                ctx.ask(self.server, 41_u32, None)
                    .map_err(|error| ExitReason::Error(format!("ask failed: {error:?}")))?,
            );
            ctx.send(ctx.actor_id(), 7_u32)
                .map_err(|error| ExitReason::Error(format!("self-send failed: {error:?}")))?;
            Ok(())
        }

        fn select_envelope<C: Context>(&mut self, ctx: &mut C) -> Option<ReceivedEnvelope> {
            if let Some(pending) = self.pending {
                return ctx.receive_selective_after(pending.mailbox_watermark, |envelope| {
                    matches!(
                        envelope,
                        Envelope::Reply { reference, .. } if *reference == pending.reference
                    ) || matches!(
                        envelope,
                        Envelope::CallTimeout(timeout) if timeout.reference == pending.reference
                    )
                });
            }

            ctx.receive_next()
        }

        fn handle<C: Context>(&mut self, envelope: Envelope, _ctx: &mut C) -> ActorTurn {
            let label = match envelope {
                Envelope::Reply { reference, message } => {
                    assert_eq!(
                        Some(reference),
                        self.pending.map(|pending| pending.reference)
                    );
                    self.pending = None;
                    format!("reply:{}", message.downcast::<u32>().ok().unwrap())
                }
                Envelope::User(payload) => {
                    format!("user:{}", payload.downcast::<u32>().ok().unwrap())
                }
                other => panic!("unexpected envelope: {other:?}"),
            };

            self.seen.lock().unwrap().push(label);
            ActorTurn::Continue
        }
    }

    #[derive(Clone, Copy)]
    enum ReceiveTimeoutMode {
        MatchBeforeTimeout,
        TimeoutWins,
    }

    struct ReceiveTimeoutActor {
        mode: ReceiveTimeoutMode,
        seen: Arc<Mutex<Vec<String>>>,
        timeout: Option<crate::ReceiveTimeout>,
    }

    impl Actor for ReceiveTimeoutActor {
        fn init<C: Context>(&mut self, ctx: &mut C) -> Result<(), ExitReason> {
            ctx.send(ctx.actor_id(), 7_u32)
                .map_err(|error| ExitReason::Error(format!("self-send failed: {error:?}")))?;

            if matches!(self.mode, ReceiveTimeoutMode::MatchBeforeTimeout) {
                ctx.send(ctx.actor_id(), 42_u32)
                    .map_err(|error| ExitReason::Error(format!("self-send failed: {error:?}")))?;
            }

            self.timeout = Some(
                ctx.arm_receive_timeout(Duration::from_millis(15))
                    .map_err(|error| {
                        ExitReason::Error(format!("timeout setup failed: {error:?}"))
                    })?,
            );
            Ok(())
        }

        fn select_envelope<C: Context>(&mut self, ctx: &mut C) -> Option<ReceivedEnvelope> {
            if let Some(timeout) = self.timeout {
                return ctx.receive_selective_with_timeout(timeout, |envelope| {
                    matches!(
                        envelope,
                        Envelope::User(payload) if payload.downcast_ref::<u32>() == Some(&42)
                    )
                });
            }

            ctx.receive_next()
        }

        fn handle<C: Context>(&mut self, envelope: Envelope, _ctx: &mut C) -> ActorTurn {
            match envelope {
                Envelope::User(payload) => {
                    let value = payload.downcast::<u32>().ok().unwrap();
                    if value == 42 {
                        self.timeout = None;
                        self.seen.lock().unwrap().push("matched:42".into());
                    } else {
                        self.seen.lock().unwrap().push(format!("user:{value}"));
                    }
                }
                Envelope::Timer(timer)
                    if self
                        .timeout
                        .is_some_and(|timeout| timeout.token() == timer.token) =>
                {
                    self.timeout = None;
                    self.seen.lock().unwrap().push("timeout".into());
                }
                other => panic!("unexpected envelope: {other:?}"),
            }

            ActorTurn::Continue
        }
    }

    struct SelectiveExitActor {
        target: crate::ActorId,
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl Actor for SelectiveExitActor {
        fn init<C: Context>(&mut self, ctx: &mut C) -> Result<(), ExitReason> {
            ctx.link(self.target)
                .map_err(|error| ExitReason::Error(format!("link failed: {error:?}")))?;
            Ok(())
        }

        fn select_envelope<C: Context>(&mut self, ctx: &mut C) -> Option<ReceivedEnvelope> {
            ctx.receive_selective(|envelope| {
                matches!(
                    envelope,
                    Envelope::User(payload) if payload.downcast_ref::<u32>() == Some(&99_u32)
                )
            })
        }

        fn handle<C: Context>(&mut self, envelope: Envelope, _ctx: &mut C) -> ActorTurn {
            if let Envelope::Exit(signal) = envelope {
                self.seen
                    .lock()
                    .unwrap()
                    .push(format!("exit:{}", signal.reason));
            } else {
                panic!("unexpected envelope: {envelope:?}");
            }

            ActorTurn::Continue
        }
    }

    struct SelfRegisteringActor {
        seen: Arc<Mutex<Option<crate::ActorId>>>,
        removed: Arc<Mutex<Option<String>>>,
    }

    impl Actor for SelfRegisteringActor {
        fn init<C: Context>(&mut self, ctx: &mut C) -> Result<(), ExitReason> {
            ctx.register_name("self-worker".into())
                .map_err(|error| ExitReason::Error(format!("register failed: {error:?}")))?;
            *self.seen.lock().unwrap() = ctx.whereis("self-worker");
            *self.removed.lock().unwrap() = ctx.unregister_name();
            Ok(())
        }

        fn handle<C: Context>(&mut self, _envelope: Envelope, _ctx: &mut C) -> ActorTurn {
            ActorTurn::Continue
        }
    }

    struct SuspendAwareActor {
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl Actor for SuspendAwareActor {
        fn handle<C: Context>(&mut self, envelope: Envelope, _ctx: &mut C) -> ActorTurn {
            let label = match envelope {
                Envelope::User(payload) => {
                    format!("user:{}", payload.downcast::<u32>().ok().unwrap())
                }
                Envelope::System(SystemMessage::Suspend) => "system:suspend".into(),
                Envelope::System(SystemMessage::Resume) => "system:resume".into(),
                Envelope::Exit(ExitSignal { reason, .. }) => format!("exit:{reason}"),
                Envelope::Down(DownMessage { reason, .. }) => format!("down:{reason}"),
                other => panic!("unexpected envelope: {other:?}"),
            };
            self.seen.lock().unwrap().push(label);
            ActorTurn::Continue
        }
    }

    struct ControlledActor {
        value: i32,
        version: u64,
    }

    impl Actor for ControlledActor {
        fn handle<C: Context>(&mut self, envelope: Envelope, _ctx: &mut C) -> ActorTurn {
            if let Envelope::User(payload) = envelope {
                self.value += payload.downcast::<i32>().ok().unwrap();
            }

            ActorTurn::Continue
        }

        fn state_version(&self) -> u64 {
            self.version
        }

        fn inspect_state<C: Context>(
            &mut self,
            _ctx: &mut C,
        ) -> Result<StateSnapshot, ControlError> {
            Ok(StateSnapshot::new(self.version, self.value))
        }

        fn replace_state<C: Context>(
            &mut self,
            snapshot: StateSnapshot,
            _ctx: &mut C,
        ) -> Result<(), ControlError> {
            self.value = match snapshot.payload.downcast::<i32>() {
                Ok(value) => value,
                Err(payload) => {
                    return Err(ControlError::invalid_state("i32", payload.type_name()));
                }
            };
            Ok(())
        }

        fn code_change<C: Context>(
            &mut self,
            target_version: u64,
            _ctx: &mut C,
        ) -> Result<(), ControlError> {
            match (self.version, target_version) {
                (current, requested) if current == requested => Ok(()),
                (0, 1) => {
                    self.value *= 10;
                    self.version = 1;
                    Ok(())
                }
                (current, requested) => Err(ControlError::VersionMismatch { current, requested }),
            }
        }
    }

    #[test]
    fn linked_exit_kills_non_trapping_actor() {
        let mut runtime = LocalRuntime::default();
        let crashing = runtime.spawn(CrashActor).unwrap();
        let linked = runtime.spawn(LinkedActor { target: crashing }).unwrap();

        runtime.run_until_idle();
        runtime.send(crashing, Control::Crash).unwrap();
        runtime.run_until_idle();

        let snapshot = runtime.actor_snapshot(linked).unwrap();
        assert_eq!(snapshot.status, crate::ActorStatus::Dead);
        assert_eq!(
            snapshot.metrics.last_exit,
            Some(ExitReason::Error("boom".into()))
        );
    }

    #[test]
    fn trap_exit_converts_linked_failures_into_messages() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = LocalRuntime::default();
        let crashing = runtime.spawn(CrashActor).unwrap();
        let trapper = runtime
            .spawn_with_options(
                TrapExitActor {
                    target: crashing,
                    seen: Arc::clone(&seen),
                },
                SpawnOptions {
                    trap_exit: true,
                    ..SpawnOptions::default()
                },
            )
            .unwrap();

        runtime.run_until_idle();
        runtime.send(crashing, Control::Crash).unwrap();
        runtime.run_until_idle();

        assert_eq!(
            seen.lock().unwrap().as_slice(),
            &[ExitReason::Error("boom".into())]
        );

        let snapshot = runtime.actor_snapshot(trapper).unwrap();
        assert_eq!(snapshot.status, crate::ActorStatus::Waiting);
    }

    #[test]
    fn monitor_on_missing_actor_enqueues_immediate_down() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = LocalRuntime::default();
        let missing = crate::ActorId::new(999, 0);

        runtime
            .spawn(MonitorMissingActor {
                missing,
                seen: Arc::clone(&seen),
            })
            .unwrap();

        runtime.run_until_idle();

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].actor, missing);
        assert_eq!(seen[0].reason, ExitReason::NoProc);
    }

    #[test]
    fn block_on_next_waits_for_tokio_timer_delivery() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let token = TimerToken::next();
        let mut runtime = LocalRuntime::default();

        runtime
            .spawn(TimerActor {
                token,
                seen: Arc::clone(&seen),
            })
            .unwrap();

        runtime.run_until_idle();
        assert!(seen.lock().unwrap().is_empty());

        assert!(runtime.block_on_next(Some(Duration::from_secs(1))));
        assert_eq!(seen.lock().unwrap().as_slice(), &[token]);
    }

    #[test]
    fn spawn_blocking_cpu_uses_tokio_pool_instead_of_actor_turn() {
        let done = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = std_mpsc::channel();
        let mut runtime = LocalRuntime::default();

        runtime
            .spawn(BlockingActor {
                done: Arc::clone(&done),
                receiver: Some(receiver),
            })
            .unwrap();

        let started = Instant::now();
        runtime.run_until_idle();
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(!done.load(Ordering::SeqCst));

        sender.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(20));

        assert!(done.load(Ordering::SeqCst));
        assert_eq!(runtime.metrics().blocking_cpu_jobs, 1);
    }

    #[test]
    fn actor_can_selectively_receive_pending_reply_after_watermark() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = LocalRuntime::default();
        let server = runtime.spawn(ReplyActor).unwrap();

        runtime
            .spawn(SelectiveReplyActor {
                server,
                seen: Arc::clone(&seen),
                pending: None,
            })
            .unwrap();

        runtime.run_until_idle();

        assert_eq!(seen.lock().unwrap().as_slice(), &["reply:42", "user:7"]);
    }

    #[test]
    fn selective_receive_timeout_fires_without_losing_older_mail() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = LocalRuntime::default();

        runtime
            .spawn(ReceiveTimeoutActor {
                mode: ReceiveTimeoutMode::TimeoutWins,
                seen: Arc::clone(&seen),
                timeout: None,
            })
            .unwrap();

        runtime.run_until_idle();
        assert!(runtime.block_on_next(Some(Duration::from_millis(100))));
        runtime.run_until_idle();

        assert_eq!(seen.lock().unwrap().as_slice(), &["timeout", "user:7"]);
    }

    #[test]
    fn selective_receive_timeout_is_cancelled_after_match() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = LocalRuntime::default();

        runtime
            .spawn(ReceiveTimeoutActor {
                mode: ReceiveTimeoutMode::MatchBeforeTimeout,
                seen: Arc::clone(&seen),
                timeout: None,
            })
            .unwrap();

        runtime.run_until_idle();
        assert_eq!(seen.lock().unwrap().as_slice(), &["matched:42", "user:7"]);

        std::thread::sleep(Duration::from_millis(30));
        assert!(!runtime.block_on_next(Some(Duration::from_millis(20))));
        assert_eq!(seen.lock().unwrap().as_slice(), &["matched:42", "user:7"]);
    }

    #[test]
    fn selective_receive_still_delivers_link_exit_signals() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = LocalRuntime::default();
        let crashing = runtime.spawn(CrashActor).unwrap();

        runtime
            .spawn_with_options(
                SelectiveExitActor {
                    target: crashing,
                    seen: Arc::clone(&seen),
                },
                SpawnOptions {
                    trap_exit: true,
                    ..SpawnOptions::default()
                },
            )
            .unwrap();

        runtime.run_until_idle();
        runtime.send(crashing, Control::Crash).unwrap();
        runtime.run_until_idle();

        assert_eq!(seen.lock().unwrap().as_slice(), &["exit:error: boom"]);
    }

    #[test]
    fn suspend_prioritizes_system_messages_and_resume_releases_user_mail() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = LocalRuntime::default();
        let actor = runtime
            .spawn(SuspendAwareActor {
                seen: Arc::clone(&seen),
            })
            .unwrap();

        runtime.send(actor, 1_u32).unwrap();
        runtime.suspend_actor(actor).unwrap();
        runtime.send(actor, 2_u32).unwrap();
        runtime.run_until_idle();

        assert_eq!(seen.lock().unwrap().as_slice(), &["system:suspend"]);
        let snapshot = runtime.actor_snapshot(actor).unwrap();
        assert!(snapshot.suspended);
        assert_eq!(snapshot.metrics.mailbox_len, 2);

        runtime.resume_actor(actor).unwrap();
        runtime.run_until_idle();

        assert_eq!(
            seen.lock().unwrap().as_slice(),
            &["system:suspend", "system:resume", "user:1", "user:2"]
        );
        let snapshot = runtime.actor_snapshot(actor).unwrap();
        assert!(!snapshot.suspended);
        assert_eq!(snapshot.metrics.mailbox_len, 0);
    }

    #[test]
    fn suspended_actor_still_receives_exit_signals() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = LocalRuntime::default();
        let actor = runtime
            .spawn_with_options(
                SuspendAwareActor {
                    seen: Arc::clone(&seen),
                },
                SpawnOptions {
                    trap_exit: true,
                    ..SpawnOptions::default()
                },
            )
            .unwrap();

        runtime.suspend_actor(actor).unwrap();
        runtime
            .send_envelope(
                actor,
                Envelope::Exit(ExitSignal {
                    from: actor,
                    reason: ExitReason::Shutdown,
                    linked: true,
                }),
            )
            .unwrap();
        runtime.send(actor, 7_u32).unwrap();
        runtime.run_until_idle();

        assert_eq!(
            seen.lock().unwrap().as_slice(),
            &["system:suspend", "exit:shutdown"]
        );
        let snapshot = runtime.actor_snapshot(actor).unwrap();
        assert!(snapshot.suspended);
        assert_eq!(snapshot.metrics.mailbox_len, 1);
    }

    #[test]
    fn suspended_actor_still_receives_down_notifications() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = LocalRuntime::default();
        let actor = runtime
            .spawn(SuspendAwareActor {
                seen: Arc::clone(&seen),
            })
            .unwrap();

        runtime.suspend_actor(actor).unwrap();
        runtime
            .send_envelope(
                actor,
                Envelope::Down(DownMessage {
                    reference: Ref::new(99),
                    actor: crate::ActorId::new(77, 0),
                    reason: ExitReason::Shutdown,
                }),
            )
            .unwrap();
        runtime.send(actor, 9_u32).unwrap();
        runtime.run_until_idle();

        assert_eq!(
            seen.lock().unwrap().as_slice(),
            &["system:suspend", "down:shutdown"]
        );
        let snapshot = runtime.actor_snapshot(actor).unwrap();
        assert!(snapshot.suspended);
        assert_eq!(snapshot.metrics.mailbox_len, 1);
    }

    #[test]
    fn bounded_mailbox_returns_explicit_backpressure() {
        let mut runtime = LocalRuntime::default();
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
        let mut runtime = LocalRuntime::new(SchedulerConfig {
            max_actors: 1,
            ..SchedulerConfig::default()
        });

        runtime.spawn(SinkActor).unwrap();
        assert_eq!(runtime.spawn(SinkActor), Err(SpawnError::CapacityExceeded));
    }

    #[test]
    fn blocking_task_completion_is_delivered_to_actor_mailbox() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = LocalRuntime::default();

        runtime
            .spawn(TaskCompletionActor {
                seen: Arc::clone(&seen),
                expected: None,
            })
            .unwrap();

        runtime.run_until_idle();
        if seen.lock().unwrap().is_empty() {
            assert!(runtime.block_on_next(Some(Duration::from_secs(1))));
        }

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].1, PoolKind::BlockingCpu);
        assert_eq!(seen[0].2, 42);
    }

    #[test]
    fn ask_timeout_is_delivered_back_to_the_caller() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = LocalRuntime::default();
        let server = runtime.spawn(NeverReplyActor).unwrap();

        runtime
            .spawn(TimeoutClient {
                server,
                seen: Arc::clone(&seen),
                pending: None,
            })
            .unwrap();

        runtime.run_until_idle();
        if seen.lock().unwrap().is_empty() {
            assert!(runtime.block_on_next(Some(Duration::from_secs(1))));
        }
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn registry_api_registers_resolves_and_unregisters_names() {
        let mut runtime = LocalRuntime::default();
        let first = runtime.spawn(SinkActor).unwrap();
        let second = runtime.spawn(SinkActor).unwrap();

        runtime.register_name(first, "worker".into()).unwrap();
        assert_eq!(runtime.resolve_name("worker"), Some(first));
        assert_eq!(
            runtime.register_name(second, "worker".into()),
            Err(RegistryError::NameTaken {
                name: "worker".into(),
                actor: first,
            })
        );
        assert_eq!(runtime.unregister_name(first).as_deref(), Some("worker"));
        assert_eq!(runtime.resolve_name("worker"), None);
    }

    #[test]
    fn actors_can_register_and_unregister_themselves_through_context() {
        let seen = Arc::new(Mutex::new(None));
        let removed = Arc::new(Mutex::new(None));
        let mut runtime = LocalRuntime::default();
        let actor = runtime
            .spawn(SelfRegisteringActor {
                seen: Arc::clone(&seen),
                removed: Arc::clone(&removed),
            })
            .unwrap();

        runtime.run_until_idle();

        assert_eq!(*seen.lock().unwrap(), Some(actor));
        assert_eq!(removed.lock().unwrap().as_deref(), Some("self-worker"));
        assert_eq!(runtime.resolve_name("self-worker"), None);
    }

    #[test]
    fn observability_introspection_exposes_tree_links_monitors_and_metrics() {
        let mut runtime = LocalRuntime::default();
        let root = runtime
            .spawn_with_options(
                SinkActor,
                SpawnOptions {
                    registered_name: Some("root".into()),
                    ..SpawnOptions::default()
                },
            )
            .unwrap();
        let linked = runtime
            .spawn_with_options(
                LinkedActor { target: root },
                SpawnOptions {
                    parent: Some(root),
                    ancestors: vec![root],
                    ..SpawnOptions::default()
                },
            )
            .unwrap();
        let monitor = runtime
            .spawn_with_options(
                MonitoringActor { target: root },
                SpawnOptions {
                    parent: Some(root),
                    ancestors: vec![root],
                    ..SpawnOptions::default()
                },
            )
            .unwrap();

        runtime.run_until_idle();

        let introspection = runtime.introspection();
        assert_eq!(introspection.actors.len(), 3);

        let root_snapshot = introspection
            .actors
            .iter()
            .find(|snapshot| snapshot.id == root)
            .unwrap();
        assert!(root_snapshot.links.contains(&linked));
        assert!(
            root_snapshot
                .monitors_in
                .iter()
                .any(|(_, actor)| *actor == monitor)
        );
        assert_eq!(root_snapshot.metrics.mailbox_len, 0);

        let linked_snapshot = introspection
            .actors
            .iter()
            .find(|snapshot| snapshot.id == linked)
            .unwrap();
        assert_eq!(linked_snapshot.parent, Some(root));
        assert!(linked_snapshot.links.contains(&root));

        let monitor_snapshot = introspection
            .actors
            .iter()
            .find(|snapshot| snapshot.id == monitor)
            .unwrap();
        assert!(
            monitor_snapshot
                .monitors_out
                .iter()
                .any(|(_, actor)| *actor == root)
        );

        let root_node = introspection
            .actor_tree
            .nodes
            .iter()
            .find(|node| node.actor.id == root)
            .unwrap();
        assert_eq!(introspection.actor_tree.roots, vec![root]);
        assert_eq!(root_node.children, vec![linked, monitor]);

        let metrics = runtime.metrics_snapshot();
        assert_eq!(metrics.live_actors, 3);
        assert_eq!(metrics.waiting_actors, 3);
        assert_eq!(metrics.total_mailbox_len, 0);

        let prometheus = runtime.export_metrics_prometheus();
        assert!(prometheus.contains("lamport_runtime_live_actors 3"));
        assert!(prometheus.contains("lamport_scheduler_utilization_ratio"));
    }

    #[test]
    fn observability_event_log_and_crash_reports_include_context() {
        let mut runtime = LocalRuntime::default();
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
                CrashActor,
                SpawnOptions {
                    registered_name: Some("child".into()),
                    parent: Some(parent),
                    ancestors: vec![root, parent],
                    ..SpawnOptions::default()
                },
            )
            .unwrap();

        runtime.run_until_idle();
        let mut cursor = runtime.event_cursor();

        runtime.send(child, Control::Crash).unwrap();
        runtime.run_until_idle();

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

        let crash_reports = runtime.crash_reports();
        let crash = crash_reports
            .iter()
            .find(|report| report.actor == child)
            .unwrap();
        assert_eq!(crash.parent, Some(parent));
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

        let mut from_start = EventCursor::from_start();
        let all_events = runtime.events_since(&mut from_start);
        assert!(all_events.len() >= runtime.event_log().len());
    }

    #[test]
    fn control_plane_supports_state_tracing_code_change_and_external_shutdown() {
        let mut runtime = LocalRuntime::default();
        let actor = runtime
            .spawn(ControlledActor {
                value: 1,
                version: 0,
            })
            .unwrap();

        runtime.send(actor, 2_i32).unwrap();
        let snapshot = runtime.get_state(actor).unwrap();
        assert_eq!(snapshot.version, 0);
        assert_eq!(snapshot.payload.downcast::<i32>().ok().unwrap(), 1);

        runtime.run_until_idle();
        let snapshot = runtime.get_state(actor).unwrap();
        assert_eq!(snapshot.payload.downcast::<i32>().ok().unwrap(), 3);

        runtime
            .replace_state(actor, StateSnapshot::new(0, 10_i32))
            .unwrap();
        let replaced = runtime.get_state(actor).unwrap();
        assert_eq!(replaced.payload.downcast::<i32>().ok().unwrap(), 10);

        let invalid = runtime.replace_state(actor, StateSnapshot::new(0, "bad"));
        assert!(matches!(
            invalid,
            Err(ControlError::InvalidState {
                expected: "i32",
                actual: "&str"
            })
        ));

        runtime.code_change_actor(actor, 1).unwrap();
        let migrated = runtime.get_state(actor).unwrap();
        assert_eq!(migrated.version, 1);
        assert_eq!(migrated.payload.downcast::<i32>().ok().unwrap(), 100);

        let rejected = runtime.code_change_actor(actor, 2);
        assert!(matches!(
            rejected,
            Err(ControlError::VersionMismatch {
                current: 1,
                requested: 2
            })
        ));

        runtime
            .trace_actor(actor, TraceOptions::messages())
            .unwrap();
        runtime.send(actor, 1_i32).unwrap();
        runtime.run_until_idle();
        runtime.untrace_actor(actor).unwrap();

        let events = runtime.event_log();
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            RuntimeEventKind::Trace(trace)
                if matches!(trace.kind, TraceEventKind::TraceEnabled { .. }) && trace.actor == actor
        )));
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            RuntimeEventKind::Trace(trace)
                if matches!(
                    trace.kind,
                    TraceEventKind::Received {
                        envelope_kind: crate::EnvelopeKind::User
                    }
                ) && trace.actor == actor
        )));
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            RuntimeEventKind::Trace(trace)
                if matches!(trace.kind, TraceEventKind::TraceDisabled) && trace.actor == actor
        )));

        runtime.shutdown_actor(actor, Shutdown::Infinity).unwrap();
        runtime.run_until_idle();
        assert_eq!(
            runtime.actor_snapshot(actor).unwrap().status,
            crate::ActorStatus::Dead
        );
    }
}
