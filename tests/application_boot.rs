use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

mod support;

use lamport::{
    Actor, ActorId, ActorStatus, ActorTurn, Application, CallOutcome, CastMessage, ChildSpec,
    ConcurrentRuntime, Context, ControlError, Envelope, ExitReason, GenServer, LocalRuntime,
    LocalUpgradeFailure, LocalUpgradeStage, ReplyToken, Restart, ServerOutcome, Shutdown,
    SpawnOptions, StartChildError, Strategy, Supervisor, SupervisorDirective, SupervisorFlags,
    behaviour::RuntimeInfo, boot_concurrent_application, boot_local_application, restart_scope,
};
use support::{expect_downcast, wait_until_concurrent};

const ROOT_NAME: &str = "integration.root";
const WORKER_NAME: &str = "integration.worker";
const WORKER_CHILD: &str = "worker";
const UPGRADE_BRANCH_CHILD: &str = "branch";
const UPGRADE_WORKER_CHILD: &str = "upgrade-worker";

#[derive(Debug, Clone, PartialEq, Eq)]
enum WorkerCall {
    Generation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WorkerCast {
    Crash,
}

struct ProbeWorker {
    starts: Arc<AtomicUsize>,
}

impl GenServer for ProbeWorker {
    type State = usize;
    type Call = WorkerCall;
    type Cast = WorkerCast;
    type Reply = usize;
    type Info = RuntimeInfo;

    fn init<C: Context>(&mut self, _ctx: &mut C) -> Result<Self::State, ExitReason> {
        Ok(self.starts.fetch_add(1, Ordering::SeqCst) + 1)
    }

    fn handle_call<C: Context>(
        &mut self,
        state: &mut Self::State,
        _from: ReplyToken,
        message: Self::Call,
        _ctx: &mut C,
    ) -> CallOutcome<Self::Reply> {
        match message {
            WorkerCall::Generation => CallOutcome::Reply(*state),
        }
    }

    fn handle_cast<C: Context>(
        &mut self,
        _state: &mut Self::State,
        message: Self::Cast,
        _ctx: &mut C,
    ) -> ServerOutcome {
        match message {
            WorkerCast::Crash => ServerOutcome::Stop(ExitReason::Error("boom".into())),
        }
    }

    fn handle_info<C: Context>(
        &mut self,
        _state: &mut Self::State,
        _message: Self::Info,
        _ctx: &mut C,
    ) -> ServerOutcome {
        ServerOutcome::Continue
    }
}

struct ClientActor {
    server: ActorId,
    seen: Arc<Mutex<Vec<usize>>>,
}

impl Actor for ClientActor {
    fn init<C: Context>(&mut self, ctx: &mut C) -> Result<(), ExitReason> {
        ctx.ask(self.server, WorkerCall::Generation, None)
            .map_err(|error| ExitReason::Error(format!("ask failed: {error:?}")))?;
        Ok(())
    }

    fn handle<C: Context>(&mut self, envelope: Envelope, _ctx: &mut C) -> ActorTurn {
        if let Envelope::Reply { message, .. } = envelope {
            self.seen
                .lock()
                .unwrap()
                .push(expect_downcast::<usize>(message, "generation reply"));
        }

        ActorTurn::Continue
    }
}

struct IntegrationSupervisor {
    flags: SupervisorFlags,
    specs: Vec<ChildSpec>,
    starts: Arc<AtomicUsize>,
}

impl IntegrationSupervisor {
    fn new(starts: Arc<AtomicUsize>) -> Self {
        Self {
            flags: SupervisorFlags {
                strategy: Strategy::OneForOne,
                ..SupervisorFlags::default()
            },
            specs: vec![ChildSpec {
                id: WORKER_CHILD,
                restart: Restart::Permanent,
                shutdown: Shutdown::default(),
                is_supervisor: false,
            }],
            starts,
        }
    }
}

impl Supervisor for IntegrationSupervisor {
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
    ) -> Result<ActorId, StartChildError> {
        match spec.id {
            WORKER_CHILD => ctx
                .spawn_gen_server(
                    ProbeWorker {
                        starts: Arc::clone(&self.starts),
                    },
                    SpawnOptions {
                        registered_name: Some(WORKER_NAME.into()),
                        ..SpawnOptions::default()
                    },
                )
                .map_err(|_| StartChildError::SpawnRejected),
            _ => Err(StartChildError::InitFailed(ExitReason::Error(format!(
                "unknown child `{}`",
                spec.id
            )))),
        }
    }

    fn on_child_exit<C: Context>(
        &mut self,
        spec: &ChildSpec,
        _actor: ActorId,
        reason: ExitReason,
        _ctx: &mut C,
    ) -> SupervisorDirective {
        if spec.should_restart(&reason) {
            SupervisorDirective::Restart(restart_scope(self.flags.strategy, &self.specs, spec.id))
        } else {
            SupervisorDirective::Ignore
        }
    }
}

struct IntegrationApplication {
    starts: Arc<AtomicUsize>,
}

impl Application for IntegrationApplication {
    type RootSupervisor = IntegrationSupervisor;

    fn name(&self) -> &'static str {
        "integration-app"
    }

    fn root_options(&self) -> SpawnOptions {
        SpawnOptions {
            registered_name: Some(ROOT_NAME.into()),
            ..SpawnOptions::default()
        }
    }

    fn root_supervisor(self) -> Self::RootSupervisor {
        IntegrationSupervisor::new(self.starts)
    }
}

fn child_actor(runtime: &mut LocalRuntime, supervisor: ActorId) -> ActorId {
    let snapshot = runtime
        .supervisor_snapshot(supervisor)
        .expect("supervisor snapshot should exist");
    let child = snapshot
        .children
        .iter()
        .find(|child| child.spec.id == WORKER_CHILD)
        .expect("worker child should exist");

    child.actor.expect("worker should be running")
}

fn request_generation(runtime: &mut LocalRuntime, server: ActorId) -> usize {
    let replies = Arc::new(Mutex::new(Vec::new()));
    runtime
        .spawn(ClientActor {
            server,
            seen: Arc::clone(&replies),
        })
        .expect("client should spawn");
    runtime.run_until_idle();

    let replies = replies.lock().unwrap();
    *replies
        .first()
        .expect("client should receive one generation reply")
}

fn supervisor_child_actor(
    runtime: &mut LocalRuntime,
    supervisor: ActorId,
    child_id: &'static str,
) -> ActorId {
    let snapshot = runtime
        .supervisor_snapshot(supervisor)
        .expect("supervisor snapshot should exist");
    snapshot
        .children
        .iter()
        .find(|child| child.spec.id == child_id)
        .and_then(|child| child.actor)
        .expect("requested child should be running")
}

fn concurrent_supervisor_child_actor(
    runtime: &ConcurrentRuntime,
    supervisor: ActorId,
    child_id: &'static str,
) -> ActorId {
    let found = Arc::new(Mutex::new(None));
    wait_until_concurrent(
        runtime,
        Duration::from_secs(1),
        &format!("supervisor child `{child_id}` to boot"),
        |runtime| {
            let actor = runtime
                .supervisor_snapshot(supervisor)
                .and_then(|snapshot| {
                    snapshot
                        .children
                        .iter()
                        .find(|child| child.spec.id == child_id)
                        .and_then(|child| child.actor)
                });
            if let Some(actor) = actor {
                *found.lock().unwrap() = Some(actor);
                true
            } else {
                false
            }
        },
    );
    found
        .lock()
        .unwrap()
        .expect("requested concurrent child should be running")
}

#[test]
fn booted_application_manages_and_restarts_a_public_supervised_subtree() {
    let starts = Arc::new(AtomicUsize::new(0));
    let (mut runtime, handle) = boot_local_application(
        IntegrationApplication {
            starts: Arc::clone(&starts),
        },
        Default::default(),
    )
    .expect("application should boot");

    runtime.run_until_idle();

    let root = handle.root_supervisor();
    let worker_before = child_actor(&mut runtime, root);
    assert_eq!(handle.name(), "integration-app");
    assert_eq!(runtime.resolve_name(ROOT_NAME), Some(root.into()));
    assert_eq!(
        runtime.resolve_name(WORKER_NAME),
        Some(worker_before.into())
    );
    assert_eq!(request_generation(&mut runtime, worker_before), 1);
    assert_eq!(starts.load(Ordering::SeqCst), 1);

    runtime
        .send(worker_before, CastMessage(WorkerCast::Crash))
        .expect("crash signal should deliver");
    runtime.run_until_idle();

    let worker_after = child_actor(&mut runtime, root);
    assert_ne!(worker_before, worker_after);
    assert_eq!(runtime.resolve_name(WORKER_NAME), Some(worker_after.into()));
    assert_eq!(starts.load(Ordering::SeqCst), 2);
    assert_eq!(request_generation(&mut runtime, worker_after), 2);

    let old_snapshot = runtime
        .actor_snapshot(worker_before)
        .expect("old worker snapshot should be retained");
    assert_eq!(old_snapshot.status, ActorStatus::Dead);

    assert!(runtime.lifecycle_events().iter().any(|event| {
        matches!(
            event,
            lamport::LifecycleEvent::Restart {
                supervisor,
                child_id,
                old_actor,
                new_actor,
            } if *supervisor == root
                && *child_id == WORKER_CHILD
                && *old_actor == Some(worker_before)
                && *new_actor == worker_after
        )
    }));

    assert!(runtime.crash_reports().iter().any(|report| {
        report.actor == worker_before
            && report.supervisor_child == Some(WORKER_CHILD)
            && report.reason == ExitReason::Error("boom".into())
    }));
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum UpgradeCast {
    Add(usize),
}

struct UpgradeWorker {
    order: Arc<Mutex<Vec<&'static str>>>,
    reject_upgrade: bool,
}

impl GenServer for UpgradeWorker {
    type State = usize;
    type Call = ();
    type Cast = UpgradeCast;
    type Reply = ();
    type Info = RuntimeInfo;

    fn init<C: Context>(&mut self, _ctx: &mut C) -> Result<Self::State, ExitReason> {
        Ok(1)
    }

    fn handle_call<C: Context>(
        &mut self,
        _state: &mut Self::State,
        _from: ReplyToken,
        _message: Self::Call,
        _ctx: &mut C,
    ) -> CallOutcome<Self::Reply> {
        CallOutcome::Reply(())
    }

    fn handle_cast<C: Context>(
        &mut self,
        state: &mut Self::State,
        message: Self::Cast,
        _ctx: &mut C,
    ) -> ServerOutcome {
        match message {
            UpgradeCast::Add(value) => *state += value,
        }
        ServerOutcome::Continue
    }

    fn handle_info<C: Context>(
        &mut self,
        _state: &mut Self::State,
        _message: Self::Info,
        _ctx: &mut C,
    ) -> ServerOutcome {
        ServerOutcome::Continue
    }

    fn inspect_state<C: Context>(
        &mut self,
        state: &mut Self::State,
        _ctx: &mut C,
    ) -> Result<lamport::Payload, ControlError> {
        Ok(lamport::Payload::new(*state))
    }

    fn code_change<C: Context>(
        &mut self,
        state: &mut Self::State,
        from_version: u64,
        to_version: u64,
        _ctx: &mut C,
    ) -> Result<(), ControlError> {
        self.order.lock().unwrap().push("worker");
        if self.reject_upgrade {
            return Err(ControlError::rejected(
                "CodeChange",
                "worker rejected the upgrade",
            ));
        }

        match (from_version, to_version) {
            (current, requested) if current == requested => Ok(()),
            (0, 1) => {
                *state *= 10;
                Ok(())
            }
            (current, requested) => Err(ControlError::VersionMismatch { current, requested }),
        }
    }
}

struct UpgradeBranchSupervisor {
    flags: SupervisorFlags,
    specs: Vec<ChildSpec>,
    order: Arc<Mutex<Vec<&'static str>>>,
    reject_worker_upgrade: bool,
    reject_branch_upgrade: bool,
    version: u64,
}

impl UpgradeBranchSupervisor {
    fn new(
        order: Arc<Mutex<Vec<&'static str>>>,
        reject_worker_upgrade: bool,
        reject_branch_upgrade: bool,
    ) -> Self {
        Self {
            flags: SupervisorFlags {
                strategy: Strategy::OneForOne,
                ..SupervisorFlags::default()
            },
            specs: vec![ChildSpec {
                id: UPGRADE_WORKER_CHILD,
                restart: Restart::Permanent,
                shutdown: Shutdown::default(),
                is_supervisor: false,
            }],
            order,
            reject_worker_upgrade,
            reject_branch_upgrade,
            version: 0,
        }
    }
}

impl Supervisor for UpgradeBranchSupervisor {
    fn flags(&self) -> SupervisorFlags {
        self.flags
    }

    fn child_specs(&self) -> &[ChildSpec] {
        &self.specs
    }

    fn state_version(&self) -> u64 {
        self.version
    }

    fn start_child<C: Context>(
        &mut self,
        spec: &ChildSpec,
        ctx: &mut C,
    ) -> Result<ActorId, StartChildError> {
        match spec.id {
            UPGRADE_WORKER_CHILD => ctx
                .spawn_gen_server(
                    UpgradeWorker {
                        order: Arc::clone(&self.order),
                        reject_upgrade: self.reject_worker_upgrade,
                    },
                    SpawnOptions::default(),
                )
                .map_err(|_| StartChildError::SpawnRejected),
            _ => Err(StartChildError::UnknownChild(spec.id)),
        }
    }

    fn on_child_exit<C: Context>(
        &mut self,
        spec: &ChildSpec,
        _actor: ActorId,
        reason: ExitReason,
        _ctx: &mut C,
    ) -> SupervisorDirective {
        if spec.should_restart(&reason) {
            SupervisorDirective::Restart(restart_scope(self.flags.strategy, &self.specs, spec.id))
        } else {
            SupervisorDirective::Ignore
        }
    }

    fn code_change<C: Context>(
        &mut self,
        from_version: u64,
        to_version: u64,
        _ctx: &mut C,
    ) -> Result<(), ControlError> {
        self.order.lock().unwrap().push("branch-supervisor");
        if self.reject_branch_upgrade {
            return Err(ControlError::rejected(
                "CodeChange",
                "branch supervisor rejected the upgrade",
            ));
        }

        match (from_version, to_version) {
            (current, requested) if current == requested => Ok(()),
            (0, 1) => {
                self.version = 1;
                Ok(())
            }
            (current, requested) => Err(ControlError::VersionMismatch { current, requested }),
        }
    }
}

struct UpgradeRootSupervisor {
    flags: SupervisorFlags,
    specs: Vec<ChildSpec>,
    order: Arc<Mutex<Vec<&'static str>>>,
    reject_worker_upgrade: bool,
    reject_branch_upgrade: bool,
    version: u64,
}

impl UpgradeRootSupervisor {
    fn new(
        order: Arc<Mutex<Vec<&'static str>>>,
        reject_worker_upgrade: bool,
        reject_branch_upgrade: bool,
    ) -> Self {
        Self {
            flags: SupervisorFlags {
                strategy: Strategy::OneForOne,
                ..SupervisorFlags::default()
            },
            specs: vec![ChildSpec {
                id: UPGRADE_BRANCH_CHILD,
                restart: Restart::Permanent,
                shutdown: Shutdown::default(),
                is_supervisor: true,
            }],
            order,
            reject_worker_upgrade,
            reject_branch_upgrade,
            version: 0,
        }
    }
}

impl Supervisor for UpgradeRootSupervisor {
    fn flags(&self) -> SupervisorFlags {
        self.flags
    }

    fn child_specs(&self) -> &[ChildSpec] {
        &self.specs
    }

    fn state_version(&self) -> u64 {
        self.version
    }

    fn start_child<C: Context>(
        &mut self,
        spec: &ChildSpec,
        ctx: &mut C,
    ) -> Result<ActorId, StartChildError> {
        match spec.id {
            UPGRADE_BRANCH_CHILD => ctx
                .spawn_supervisor(
                    UpgradeBranchSupervisor::new(
                        Arc::clone(&self.order),
                        self.reject_worker_upgrade,
                        self.reject_branch_upgrade,
                    ),
                    SpawnOptions::default(),
                )
                .map_err(|_| StartChildError::SpawnRejected),
            _ => Err(StartChildError::UnknownChild(spec.id)),
        }
    }

    fn on_child_exit<C: Context>(
        &mut self,
        spec: &ChildSpec,
        _actor: ActorId,
        reason: ExitReason,
        _ctx: &mut C,
    ) -> SupervisorDirective {
        if spec.should_restart(&reason) {
            SupervisorDirective::Restart(restart_scope(self.flags.strategy, &self.specs, spec.id))
        } else {
            SupervisorDirective::Ignore
        }
    }

    fn code_change<C: Context>(
        &mut self,
        from_version: u64,
        to_version: u64,
        _ctx: &mut C,
    ) -> Result<(), ControlError> {
        self.order.lock().unwrap().push("root-supervisor");
        match (from_version, to_version) {
            (current, requested) if current == requested => Ok(()),
            (0, 1) => {
                self.version = 1;
                Ok(())
            }
            (current, requested) => Err(ControlError::VersionMismatch { current, requested }),
        }
    }
}

struct UpgradeApplication {
    order: Arc<Mutex<Vec<&'static str>>>,
    reject_worker_upgrade: bool,
    reject_branch_upgrade: bool,
}

impl Application for UpgradeApplication {
    type RootSupervisor = UpgradeRootSupervisor;

    fn name(&self) -> &'static str {
        "upgrade-app"
    }

    fn root_supervisor(self) -> Self::RootSupervisor {
        UpgradeRootSupervisor::new(
            self.order,
            self.reject_worker_upgrade,
            self.reject_branch_upgrade,
        )
    }
}

#[test]
fn application_upgrade_quiesces_tree_and_runs_children_before_parents() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let (mut runtime, handle) = boot_local_application(
        UpgradeApplication {
            order: Arc::clone(&order),
            reject_worker_upgrade: false,
            reject_branch_upgrade: false,
        },
        Default::default(),
    )
    .expect("application should boot");

    runtime.run_until_idle();

    let root = handle.root_supervisor();
    let branch = supervisor_child_actor(&mut runtime, root, UPGRADE_BRANCH_CHILD);
    let worker = supervisor_child_actor(&mut runtime, branch, UPGRADE_WORKER_CHILD);

    runtime
        .send(worker, CastMessage(UpgradeCast::Add(1)))
        .expect("queued cast should deliver");

    let report = runtime.upgrade_application(handle, 1).unwrap();
    assert_eq!(report.suspend_order, vec![root, branch, worker]);
    assert_eq!(report.upgrade_order, vec![worker, branch, root]);

    let snapshot = runtime.get_state(worker).unwrap();
    assert_eq!(snapshot.version, 1);
    assert_eq!(snapshot.payload.downcast::<usize>().ok().unwrap(), 11);
    assert_eq!(
        order.lock().unwrap().as_slice(),
        &["worker", "branch-supervisor", "root-supervisor"]
    );
}

#[test]
fn application_upgrade_failure_resumes_tree_after_rejected_child_change() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let (mut runtime, handle) = boot_local_application(
        UpgradeApplication {
            order: Arc::clone(&order),
            reject_worker_upgrade: true,
            reject_branch_upgrade: false,
        },
        Default::default(),
    )
    .expect("application should boot");

    runtime.run_until_idle();

    let root = handle.root_supervisor();
    let branch = supervisor_child_actor(&mut runtime, root, UPGRADE_BRANCH_CHILD);
    let worker = supervisor_child_actor(&mut runtime, branch, UPGRADE_WORKER_CHILD);

    runtime
        .send(worker, CastMessage(UpgradeCast::Add(1)))
        .expect("queued cast should deliver");

    let error = runtime.upgrade_application(handle, 1).unwrap_err();
    assert_eq!(error.actor, Some(worker));
    assert_eq!(error.stage, Some(LocalUpgradeStage::CodeChange));
    assert_eq!(error.suspended, vec![root, branch, worker]);
    assert!(error.upgraded.is_empty());
    assert!(matches!(
        error.failure.as_ref(),
        LocalUpgradeFailure::Control(ControlError::Rejected {
            operation: "CodeChange",
            reason,
        }) if reason == "worker rejected the upgrade"
    ));

    let snapshot = runtime.get_state(worker).unwrap();
    assert_eq!(snapshot.version, 0);
    assert_eq!(snapshot.payload.downcast::<usize>().ok().unwrap(), 2);
    assert_eq!(order.lock().unwrap().as_slice(), &["worker"]);
}

#[test]
fn application_upgrade_failure_keeps_prior_code_changes_and_resumes_tree() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let (mut runtime, handle) = boot_local_application(
        UpgradeApplication {
            order: Arc::clone(&order),
            reject_worker_upgrade: false,
            reject_branch_upgrade: true,
        },
        Default::default(),
    )
    .expect("application should boot");

    runtime.run_until_idle();

    let root = handle.root_supervisor();
    let branch = supervisor_child_actor(&mut runtime, root, UPGRADE_BRANCH_CHILD);
    let worker = supervisor_child_actor(&mut runtime, branch, UPGRADE_WORKER_CHILD);

    runtime
        .send(worker, CastMessage(UpgradeCast::Add(1)))
        .expect("queued cast should deliver");

    let error = runtime.upgrade_application(handle, 1).unwrap_err();
    assert_eq!(error.actor, Some(branch));
    assert_eq!(error.stage, Some(LocalUpgradeStage::CodeChange));
    assert_eq!(error.suspended, vec![root, branch, worker]);
    assert_eq!(error.upgraded, vec![worker]);
    assert!(matches!(
        error.failure.as_ref(),
        LocalUpgradeFailure::Control(ControlError::Rejected {
            operation: "CodeChange",
            reason,
        }) if reason == "branch supervisor rejected the upgrade"
    ));

    let snapshot = runtime.get_state(worker).unwrap();
    assert_eq!(snapshot.version, 1);
    assert_eq!(snapshot.payload.downcast::<usize>().ok().unwrap(), 11);

    runtime
        .send(worker, CastMessage(UpgradeCast::Add(2)))
        .expect("post-failure cast should queue");
    runtime.run_until_idle();

    let snapshot = runtime.get_state(worker).unwrap();
    assert_eq!(snapshot.version, 1);
    assert_eq!(snapshot.payload.downcast::<usize>().ok().unwrap(), 13);
    assert_eq!(
        order.lock().unwrap().as_slice(),
        &["worker", "branch-supervisor"]
    );
}

#[test]
fn concurrent_application_upgrade_quiesces_tree_and_runs_children_before_parents() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let (runtime, handle) = boot_concurrent_application(
        UpgradeApplication {
            order: Arc::clone(&order),
            reject_worker_upgrade: false,
            reject_branch_upgrade: false,
        },
        Default::default(),
    )
    .expect("application should boot");

    let root = handle.root_supervisor();
    let branch = concurrent_supervisor_child_actor(&runtime, root, UPGRADE_BRANCH_CHILD);
    let worker = concurrent_supervisor_child_actor(&runtime, branch, UPGRADE_WORKER_CHILD);

    runtime
        .send(worker, CastMessage(UpgradeCast::Add(1)))
        .expect("queued cast should deliver");

    let report = runtime.upgrade_application(handle, 1).unwrap();
    assert_eq!(report.suspend_order, vec![root, branch, worker]);
    assert_eq!(report.upgrade_order, vec![worker, branch, root]);

    let snapshot = runtime.get_state(worker).unwrap();
    assert_eq!(snapshot.version, 1);
    assert!(matches!(
        snapshot.payload.downcast::<usize>().ok().unwrap(),
        11 | 20
    ));
    assert_eq!(
        order.lock().unwrap().as_slice(),
        &["worker", "branch-supervisor", "root-supervisor"]
    );
}

#[test]
fn concurrent_application_upgrade_failure_resumes_tree_after_rejected_child_change() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let (runtime, handle) = boot_concurrent_application(
        UpgradeApplication {
            order: Arc::clone(&order),
            reject_worker_upgrade: true,
            reject_branch_upgrade: false,
        },
        Default::default(),
    )
    .expect("application should boot");

    let root = handle.root_supervisor();
    let branch = concurrent_supervisor_child_actor(&runtime, root, UPGRADE_BRANCH_CHILD);
    let worker = concurrent_supervisor_child_actor(&runtime, branch, UPGRADE_WORKER_CHILD);

    runtime
        .send(worker, CastMessage(UpgradeCast::Add(1)))
        .expect("queued cast should deliver");

    let error = runtime.upgrade_application(handle, 1).unwrap_err();
    assert_eq!(error.actor, Some(worker));
    assert_eq!(error.stage, Some(LocalUpgradeStage::CodeChange));
    assert_eq!(error.suspended, vec![root, branch, worker]);
    assert!(error.upgraded.is_empty());
    assert!(matches!(
        error.failure.as_ref(),
        LocalUpgradeFailure::Control(ControlError::Rejected {
            operation: "CodeChange",
            reason,
        }) if reason == "worker rejected the upgrade"
    ));

    let snapshot = runtime.get_state(worker).unwrap();
    assert_eq!(snapshot.version, 0);
    assert_eq!(snapshot.payload.downcast::<usize>().ok().unwrap(), 2);
    assert_eq!(order.lock().unwrap().as_slice(), &["worker"]);
}

#[test]
fn concurrent_application_upgrade_failure_keeps_prior_code_changes_and_resumes_tree() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let (runtime, handle) = boot_concurrent_application(
        UpgradeApplication {
            order: Arc::clone(&order),
            reject_worker_upgrade: false,
            reject_branch_upgrade: true,
        },
        Default::default(),
    )
    .expect("application should boot");

    let root = handle.root_supervisor();
    let branch = concurrent_supervisor_child_actor(&runtime, root, UPGRADE_BRANCH_CHILD);
    let worker = concurrent_supervisor_child_actor(&runtime, branch, UPGRADE_WORKER_CHILD);

    runtime
        .send(worker, CastMessage(UpgradeCast::Add(1)))
        .expect("queued cast should deliver");

    let error = runtime.upgrade_application(handle, 1).unwrap_err();
    assert_eq!(error.actor, Some(branch));
    assert_eq!(error.stage, Some(LocalUpgradeStage::CodeChange));
    assert_eq!(error.suspended, vec![root, branch, worker]);
    assert_eq!(error.upgraded, vec![worker]);
    assert!(matches!(
        error.failure.as_ref(),
        LocalUpgradeFailure::Control(ControlError::Rejected {
            operation: "CodeChange",
            reason,
        }) if reason == "branch supervisor rejected the upgrade"
    ));

    let snapshot = runtime.get_state(worker).unwrap();
    assert_eq!(snapshot.version, 1);
    assert!(matches!(
        snapshot.payload.downcast::<usize>().ok().unwrap(),
        11 | 20
    ));

    runtime
        .send(worker, CastMessage(UpgradeCast::Add(2)))
        .expect("post-failure cast should queue");
    wait_until_concurrent(
        &runtime,
        Duration::from_secs(1),
        "post-upgrade cast to deliver on concurrent runtime",
        |runtime| {
            runtime.get_state(worker).is_ok_and(|snapshot| {
                snapshot.version == 1
                    && matches!(snapshot.payload.downcast::<usize>().ok(), Some(13 | 22))
            })
        },
    );

    let snapshot = runtime.get_state(worker).unwrap();
    assert_eq!(snapshot.version, 1);
    assert!(matches!(
        snapshot.payload.downcast::<usize>().ok().unwrap(),
        13 | 22
    ));
    assert_eq!(
        order.lock().unwrap().as_slice(),
        &["worker", "branch-supervisor"]
    );
}
