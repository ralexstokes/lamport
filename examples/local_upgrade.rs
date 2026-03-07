use std::sync::{Arc, Mutex};

use lamport::{
    ActorId, Application, CallOutcome, CastMessage, ChildSpec, Context, ControlError, GenServer,
    LocalRuntime, ReplyToken, Restart, ServerOutcome, Shutdown, SpawnOptions, StartChildError,
    Strategy, Supervisor, SupervisorDirective, SupervisorFlags, behaviour::RuntimeInfo,
    boot_local_application, restart_scope,
};

const ROOT_CHILD: &str = "branch";
const BRANCH_CHILD: &str = "worker";

#[derive(Debug, Clone, PartialEq, Eq)]
enum WorkerCast {
    Add(i32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkerSnapshot {
    value: i32,
}

struct UpgradeWorker {
    order: Arc<Mutex<Vec<&'static str>>>,
}

impl GenServer for UpgradeWorker {
    type State = i32;
    type Call = ();
    type Cast = WorkerCast;
    type Reply = WorkerSnapshot;
    type Info = RuntimeInfo;

    fn init<C: Context>(&mut self, _ctx: &mut C) -> Result<Self::State, lamport::ExitReason> {
        Ok(1)
    }

    fn handle_call<C: Context>(
        &mut self,
        state: &mut Self::State,
        _from: ReplyToken,
        _message: Self::Call,
        _ctx: &mut C,
    ) -> CallOutcome<Self::Reply> {
        CallOutcome::Reply(WorkerSnapshot { value: *state })
    }

    fn handle_cast<C: Context>(
        &mut self,
        state: &mut Self::State,
        message: Self::Cast,
        _ctx: &mut C,
    ) -> ServerOutcome {
        match message {
            WorkerCast::Add(delta) => *state += delta,
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

    fn state_version(&self) -> u64 {
        0
    }

    fn inspect_state<C: Context>(
        &mut self,
        state: &mut Self::State,
        _ctx: &mut C,
    ) -> Result<lamport::Payload, ControlError> {
        Ok(lamport::Payload::new(WorkerSnapshot { value: *state }))
    }

    fn code_change<C: Context>(
        &mut self,
        state: &mut Self::State,
        from_version: u64,
        to_version: u64,
        _ctx: &mut C,
    ) -> Result<(), ControlError> {
        self.order.lock().unwrap().push("worker");
        match (from_version, to_version) {
            (0, 1) => {
                *state *= 10;
                Ok(())
            }
            (current, requested) if current == requested => Ok(()),
            (current, requested) => Err(ControlError::VersionMismatch { current, requested }),
        }
    }
}

struct BranchSupervisor {
    flags: SupervisorFlags,
    specs: Vec<ChildSpec>,
    version: u64,
    order: Arc<Mutex<Vec<&'static str>>>,
}

impl BranchSupervisor {
    fn new(order: Arc<Mutex<Vec<&'static str>>>) -> Self {
        Self {
            flags: SupervisorFlags {
                strategy: Strategy::OneForOne,
                ..SupervisorFlags::default()
            },
            specs: vec![ChildSpec {
                id: BRANCH_CHILD,
                restart: Restart::Permanent,
                shutdown: Shutdown::default(),
                is_supervisor: false,
            }],
            version: 0,
            order,
        }
    }
}

impl Supervisor for BranchSupervisor {
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
            BRANCH_CHILD => ctx
                .spawn_gen_server(
                    UpgradeWorker {
                        order: Arc::clone(&self.order),
                    },
                    SpawnOptions {
                        registered_name: Some("upgrade.worker".into()),
                        ..SpawnOptions::default()
                    },
                )
                .map_err(|_| StartChildError::SpawnRejected),
            _ => Err(StartChildError::UnknownChild(spec.id)),
        }
    }

    fn on_child_exit<C: Context>(
        &mut self,
        spec: &ChildSpec,
        _actor: ActorId,
        reason: lamport::ExitReason,
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
        match (from_version, to_version) {
            (0, 1) => {
                self.version = 1;
                Ok(())
            }
            (current, requested) if current == requested => Ok(()),
            (current, requested) => Err(ControlError::VersionMismatch { current, requested }),
        }
    }
}

struct RootSupervisor {
    flags: SupervisorFlags,
    specs: Vec<ChildSpec>,
    version: u64,
    order: Arc<Mutex<Vec<&'static str>>>,
}

impl RootSupervisor {
    fn new(order: Arc<Mutex<Vec<&'static str>>>) -> Self {
        Self {
            flags: SupervisorFlags {
                strategy: Strategy::OneForOne,
                ..SupervisorFlags::default()
            },
            specs: vec![ChildSpec {
                id: ROOT_CHILD,
                restart: Restart::Permanent,
                shutdown: Shutdown::default(),
                is_supervisor: true,
            }],
            version: 0,
            order,
        }
    }
}

impl Supervisor for RootSupervisor {
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
            ROOT_CHILD => ctx
                .spawn_supervisor(
                    BranchSupervisor::new(Arc::clone(&self.order)),
                    SpawnOptions {
                        registered_name: Some("upgrade.branch".into()),
                        ..SpawnOptions::default()
                    },
                )
                .map_err(|_| StartChildError::SpawnRejected),
            _ => Err(StartChildError::UnknownChild(spec.id)),
        }
    }

    fn on_child_exit<C: Context>(
        &mut self,
        spec: &ChildSpec,
        _actor: ActorId,
        reason: lamport::ExitReason,
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
            (0, 1) => {
                self.version = 1;
                Ok(())
            }
            (current, requested) if current == requested => Ok(()),
            (current, requested) => Err(ControlError::VersionMismatch { current, requested }),
        }
    }
}

struct UpgradeApplication {
    order: Arc<Mutex<Vec<&'static str>>>,
}

impl Application for UpgradeApplication {
    type RootSupervisor = RootSupervisor;

    fn name(&self) -> &'static str {
        "local-upgrade-demo"
    }

    fn root_options(&self) -> SpawnOptions {
        SpawnOptions {
            registered_name: Some("upgrade.root".into()),
            ..SpawnOptions::default()
        }
    }

    fn root_supervisor(self) -> Self::RootSupervisor {
        RootSupervisor::new(self.order)
    }
}

fn child_actor(
    runtime: &mut LocalRuntime,
    supervisor: ActorId,
    child_id: &'static str,
) -> Result<ActorId, String> {
    let snapshot = runtime
        .supervisor_snapshot(supervisor)
        .ok_or_else(|| format!("missing supervisor snapshot for {supervisor}"))?;

    snapshot
        .children
        .iter()
        .find(|child| child.spec.id == child_id)
        .and_then(|child| child.actor)
        .ok_or_else(|| format!("missing running child `{child_id}` under {supervisor}"))
}

fn print_worker_state(
    runtime: &mut LocalRuntime,
    worker: ActorId,
    label: &str,
) -> Result<(), String> {
    let snapshot = runtime
        .get_state(worker)
        .map_err(|error| format!("get_state failed: {error}"))?;
    let version = snapshot.version;
    let state = snapshot
        .payload
        .downcast::<WorkerSnapshot>()
        .map_err(|payload| format!("unexpected worker snapshot `{}`", payload.type_name()))?;
    println!("{label}: version={version} value={}", state.value);
    Ok(())
}

fn run() -> Result<(), String> {
    let order = Arc::new(Mutex::new(Vec::new()));
    let (mut runtime, handle) = boot_local_application(
        UpgradeApplication {
            order: Arc::clone(&order),
        },
        Default::default(),
    )
    .map_err(|error| format!("boot failed: {error:?}"))?;

    runtime.run_until_idle();

    let root = handle.root_supervisor();
    let branch = child_actor(&mut runtime, root, ROOT_CHILD)?;
    let worker = child_actor(&mut runtime, branch, BRANCH_CHILD)?;

    print_worker_state(&mut runtime, worker, "before upgrade")?;

    runtime
        .send(worker, CastMessage(WorkerCast::Add(1)))
        .map_err(|error| format!("send failed: {error:?}"))?;
    println!("queued one user message before upgrade");

    let report = runtime
        .upgrade_application(handle, 1)
        .map_err(|error| format!("upgrade failed: {error}"))?;

    println!("suspend order: {:?}", report.suspend_order);
    println!("upgrade order: {:?}", report.upgrade_order);
    println!("hook order: {:?}", order.lock().unwrap().as_slice());

    print_worker_state(&mut runtime, worker, "after upgrade")?;
    println!(
        "note: value 11 means the worker migrated 1 -> 10 during code change, then the queued Add(1) message ran after resume"
    );

    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
