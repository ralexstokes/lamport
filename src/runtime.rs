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
        Context, LinkError, MonitorError, PendingCall, SendError, SpawnError, SpawnOptions,
        TaskHandle, TimerError,
    },
    envelope::{DownMessage, Envelope, ExitSignal, Message, ReplyToken, TaskCompleted, TimerFired},
    mailbox::{Mailbox, MailboxFull},
    observability::{
        ActorIdentity, ActorTree, EventCursor, RuntimeEvent, RuntimeEventKind,
        RuntimeIntrospection, RuntimeMetricsSnapshot, build_actor_tree, build_metrics_snapshot,
        build_runtime_introspection, emit_tracing_event, events_since,
    },
    registry::{Registry, RegistryError},
    scheduler::{PoolKind, RunQueueSnapshot, SchedulerConfig, SchedulerMetrics},
    supervisor::{RestartIntensity, Supervisor, SupervisorActor},
    types::{
        ActorId, ActorMetrics, ActorStatus, ChildSpec, CrashReport, ExitReason, LifecycleEvent,
        Ref, Shutdown, ShutdownPhase, SupervisorFlags, TimerToken,
    },
};

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

/// Snapshot of actor state exposed for runtime introspection and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActorSnapshot {
    /// Runtime id for the actor instance.
    pub id: ActorId,
    /// Human-readable actor name used in traces and crash reports.
    pub name: &'static str,
    /// Optional registered name.
    pub registered_name: Option<String>,
    /// Parent actor in the supervision tree.
    pub parent: Option<ActorId>,
    /// Ancestor chain captured at spawn time.
    pub ancestors: Vec<ActorId>,
    /// Link set for the actor.
    pub links: Vec<ActorId>,
    /// Actors monitoring this actor by reference.
    pub monitors_in: Vec<(Ref, ActorId)>,
    /// Actors monitored by this actor by reference.
    pub monitors_out: Vec<(Ref, ActorId)>,
    /// Whether linked exits are trapped into the mailbox.
    pub trap_exit: bool,
    /// Current lifecycle status.
    pub status: ActorStatus,
    /// Runtime metrics for the actor.
    pub metrics: ActorMetrics,
}

/// Child state tracked by the runtime for a supervisor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorChildSnapshot {
    /// Static child spec.
    pub spec: ChildSpec,
    /// Running actor id for the child, if any.
    pub actor: Option<ActorId>,
}

/// Snapshot of live supervisor metadata tracked by the runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorSnapshot {
    /// Supervisor actor id.
    pub actor: ActorId,
    /// Supervisor restart flags.
    pub flags: SupervisorFlags,
    /// Ordered child specs with current running ids.
    pub children: Vec<SupervisorChildSnapshot>,
    /// Restarts currently inside the active intensity window.
    pub active_restarts: usize,
}

#[derive(Debug, Clone)]
struct SupervisorRuntimeState {
    flags: SupervisorFlags,
    child_specs: Vec<ChildSpec>,
    running: BTreeMap<&'static str, ActorId>,
    intensity: RestartIntensity,
}

impl SupervisorRuntimeState {
    fn new(flags: SupervisorFlags, child_specs: Vec<ChildSpec>) -> Self {
        Self {
            flags: flags.clone(),
            child_specs,
            running: BTreeMap::new(),
            intensity: RestartIntensity::new(flags),
        }
    }

    fn set_running(&mut self, child_id: &'static str, actor: ActorId) {
        self.running.insert(child_id, actor);
    }

    fn clear_running(&mut self, child_id: &'static str, actor: ActorId) {
        if self.running.get(child_id).copied() == Some(actor) {
            self.running.remove(child_id);
        }
    }

    fn snapshot(&mut self, actor: ActorId) -> SupervisorSnapshot {
        SupervisorSnapshot {
            actor,
            flags: self.flags.clone(),
            children: self
                .child_specs
                .iter()
                .cloned()
                .map(|spec| SupervisorChildSnapshot {
                    actor: self.running.get(spec.id).copied(),
                    spec,
                })
                .collect(),
            active_restarts: self.intensity.active_restarts(Instant::now()),
        }
    }
}

#[derive(Debug)]
struct ShutdownTracker {
    requester: ActorId,
    policy: Shutdown,
    task: Option<JoinHandle<()>>,
}

#[derive(Debug)]
struct ActorState {
    id: ActorId,
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
    scheduled: bool,
}

impl ActorState {
    fn new(id: ActorId, options: SpawnOptions, config: &SchedulerConfig) -> Self {
        let mailbox_capacity = options
            .mailbox_capacity
            .unwrap_or(config.default_mailbox_capacity);
        let mailbox = Mailbox::with_limits(
            mailbox_capacity,
            config
                .mailbox_runtime_reserve
                .min(mailbox_capacity.saturating_sub(1)),
        );

        Self {
            id,
            mailbox,
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
            status: self.status,
            metrics: self.metrics.clone(),
        }
    }
}

trait ActorCell: Send {
    fn init(&mut self, ctx: &mut LocalContext<'_>) -> Result<(), ExitReason>;
    fn handle(&mut self, envelope: Envelope, ctx: &mut LocalContext<'_>) -> ActorTurn;
    fn terminate(&mut self, reason: ExitReason, ctx: &mut LocalContext<'_>);
}

impl<A: Actor> ActorCell for A {
    fn init(&mut self, ctx: &mut LocalContext<'_>) -> Result<(), ExitReason> {
        Actor::init(self, ctx)
    }

    fn handle(&mut self, envelope: Envelope, ctx: &mut LocalContext<'_>) -> ActorTurn {
        Actor::handle(self, envelope, ctx)
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

#[derive(Debug, Default)]
struct TurnEffects {
    exit_reason: Option<ExitReason>,
    yielded: bool,
}

struct LocalContext<'a> {
    runtime: &'a mut LocalRuntime,
    current: &'a mut ActorState,
    effects: TurnEffects,
}

impl<'a> LocalContext<'a> {
    fn new(runtime: &'a mut LocalRuntime, current: &'a mut ActorState) -> Self {
        Self {
            runtime,
            current,
            effects: TurnEffects::default(),
        }
    }

    fn finish(self) -> TurnEffects {
        self.effects
    }

    fn send_to(&mut self, to: ActorId, envelope: Envelope) -> Result<(), SendError> {
        if to == self.current.id {
            if let Some(actor_id) = push_envelope(self.current, envelope)? {
                self.runtime.run_queue.push_back(actor_id);
            }
            return Ok(());
        }

        self.runtime.enqueue_actor_envelope(to, envelope)
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
        let kill_now = matches!(signal.reason, ExitReason::Kill)
            || (signal.linked
                && !self.current.trap_exit
                && !matches!(signal.reason, ExitReason::Normal));

        if kill_now {
            self.effects.exit_reason = Some(signal.reason);
            return;
        }

        if signal.linked && !self.current.trap_exit && matches!(signal.reason, ExitReason::Normal) {
            return;
        }

        self.enqueue_self_runtime_envelope(Envelope::Exit(signal), "exit");
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
        std::mem::drop(self.runtime.tokio.spawn_blocking(move || {
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

impl Context for LocalContext<'_> {
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
        let config = normalize_config(config);

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
    pub fn lifecycle_events(&self) -> &[LifecycleEvent] {
        &self.lifecycle_events
    }

    /// Returns the accumulated abnormal crash reports.
    pub fn crash_reports(&self) -> &[CrashReport] {
        &self.crash_reports
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
        build_actor_tree(self.actor_snapshots())
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
        self.metrics.clone()
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

        match policy.clone() {
            Shutdown::BrutalKill => {
                self.shutdown_tasks.insert(
                    actor,
                    ShutdownTracker {
                        requester,
                        policy,
                        task: None,
                    },
                );
                self.force_exit(actor, ExitReason::Kill);
            }
            Shutdown::Timeout(delay) => {
                let task = self.spawn_shutdown_timer(actor, delay);
                self.begin_linked_shutdown(requester, actor, policy, Some(task));
            }
            Shutdown::Infinity => {
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
        self.shutdown_tasks.insert(
            actor,
            ShutdownTracker {
                requester,
                policy,
                task,
            },
        );
        self.propagate_link_exit(
            actor,
            ExitSignal {
                from: requester,
                reason: ExitReason::Shutdown,
                linked: true,
            },
        );
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

        let Some(envelope) = entry.state.mailbox.pop_front() else {
            park_actor(&mut entry.state);
            self.actors.insert(entry.state.id.local_id, entry);
            return;
        };

        entry.state.metrics.mailbox_len = entry.state.mailbox.len();
        entry.state.status = ActorStatus::Running;
        entry.state.metrics.scheduler_id = Some(0);
        entry.state.metrics.turns_run += 1;
        self.metrics.normal_turns += 1;

        let (turn_result, effects) = {
            let actor_id = entry.state.id;
            let actor_name = entry.name;
            let envelope_kind = envelope.kind();
            let mut ctx = LocalContext::new(self, &mut entry.state);
            let turn_span = tracing::trace_span!(
                "lamport.actor.turn",
                actor_id = %actor_id,
                actor_name = actor_name,
                scheduler_id = 0usize,
                envelope_kind = ?envelope_kind
            );
            let _turn_guard = turn_span.enter();
            let result = catch_unwind(AssertUnwindSafe(|| entry.actor.handle(envelope, &mut ctx)));
            let effects = ctx.finish();
            (result, effects)
        };

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
                let name = entry.name;
                self.finish_actor(entry, panic_reason(name, panic));
            }
        }
    }

    fn run_init(&mut self, mut entry: ActorEntry) {
        entry.state.status = ActorStatus::Running;
        entry.state.metrics.scheduler_id = Some(0);

        let (init_result, effects) = {
            let actor_id = entry.state.id;
            let actor_name = entry.name;
            let mut ctx = LocalContext::new(self, &mut entry.state);
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
                let name = entry.name;
                self.finish_actor(entry, panic_reason(name, panic));
            }
        }
    }

    fn return_actor(&mut self, mut entry: ActorEntry) {
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

    fn finish_actor(&mut self, mut entry: ActorEntry, reason: ExitReason) {
        entry.state.status = ActorStatus::Exiting;
        entry.state.metrics.scheduler_id = Some(0);

        let terminate_result = {
            let actor_id = entry.state.id;
            let actor_name = entry.name;
            let mut ctx = LocalContext::new(self, &mut entry.state);
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
            Err(panic) => panic_reason(entry.name, panic),
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
        let linked_reason = if self.shutdown_tasks.contains_key(&actor_id)
            && matches!(final_reason, ExitReason::Kill)
        {
            ExitReason::Shutdown
        } else {
            final_reason.clone()
        };

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
        self.live_actors = self.live_actors.saturating_sub(1);
    }

    fn remove_outgoing_monitors(&mut self, actor: ActorId, monitoring: Vec<(Ref, ActorId)>) {
        for (reference, target) in monitoring {
            if target == actor {
                continue;
            }

            if let Some(target_state) = self.actor_state_mut(target) {
                target_state.monitors_in.remove(&reference);
            }
        }
    }

    fn notify_incoming_monitors(
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
                        actor,
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

    fn detach_links_and_propagate(
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
                    from: actor,
                    reason: reason.clone(),
                    linked: true,
                },
            );
        }
    }

    fn propagate_link_exit(&mut self, target: ActorId, signal: ExitSignal) {
        let Some((kill_now, trap_exit)) = self.actor_state_mut(target).map(|target_state| {
            (
                matches!(signal.reason, ExitReason::Kill)
                    || (signal.linked
                        && !target_state.trap_exit
                        && !matches!(signal.reason, ExitReason::Normal)),
                target_state.trap_exit,
            )
        }) else {
            return;
        };

        if kill_now {
            self.force_exit(target, signal.reason);
            return;
        }

        if signal.linked && !trap_exit && matches!(signal.reason, ExitReason::Normal) {
            return;
        }

        let _ = self.enqueue_runtime_envelope(target, Envelope::Exit(signal), "exit");
    }

    fn force_exit(&mut self, actor: ActorId, reason: ExitReason) -> bool {
        let Some(entry) = self.take_actor(actor) else {
            return false;
        };

        self.finish_actor(entry, reason);
        true
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
        let Some(handle) = self.timer_tasks.remove(&(actor, token)) else {
            return false;
        };

        handle.abort();
        true
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

fn normalize_config(mut config: SchedulerConfig) -> SchedulerConfig {
    config.scheduler_count = 1;
    config.max_actors = config.max_actors.max(1);
    config.default_mailbox_capacity = config.default_mailbox_capacity.max(1);
    config.mailbox_runtime_reserve = config
        .mailbox_runtime_reserve
        .min(config.default_mailbox_capacity.saturating_sub(1));
    config
}

fn schedule_actor(run_queue: &mut VecDeque<ActorId>, state: &mut ActorState) {
    if let Some(actor_id) = mark_scheduled(state) {
        run_queue.push_back(actor_id);
    }
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

fn panic_reason(name: &'static str, payload: Box<dyn std::any::Any + Send>) -> ExitReason {
    let detail = payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| {
            payload
                .downcast_ref::<&'static str>()
                .map(|message| (*message).to_owned())
        })
        .unwrap_or_else(|| "unknown panic".to_owned());

    ExitReason::Error(format!("actor `{name}` panicked: {detail}"))
}

fn mailbox_overflow_reason(actor: ActorId, label: &str) -> ExitReason {
    ExitReason::Error(format!(
        "actor `{actor:?}` mailbox overflow while delivering {label}"
    ))
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
        Actor, ActorTurn, Context, Envelope, EventCursor, ExitReason, LifecycleEvent, LocalRuntime,
        PoolKind, Ref, RegistryError, RuntimeEventKind, SchedulerConfig, SendError, SpawnError,
        SpawnOptions, TimerToken, envelope::DownMessage,
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

        let crash = runtime
            .crash_reports()
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
}
