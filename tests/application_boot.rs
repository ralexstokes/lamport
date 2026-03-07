use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

mod support;

use lamport::{
    Actor, ActorId, ActorStatus, ActorTurn, Application, CallOutcome, CastMessage, ChildSpec,
    Context, Envelope, ExitReason, GenServer, LocalRuntime, ReplyToken, Restart, ServerOutcome,
    Shutdown, SpawnOptions, StartChildError, Strategy, Supervisor, SupervisorDirective,
    SupervisorFlags, behaviour::RuntimeInfo, boot_local_application, restart_scope,
};
use support::expect_downcast;

const ROOT_NAME: &str = "integration.root";
const WORKER_NAME: &str = "integration.worker";
const WORKER_CHILD: &str = "worker";

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
    assert_eq!(runtime.resolve_name(ROOT_NAME), Some(root));
    assert_eq!(runtime.resolve_name(WORKER_NAME), Some(worker_before));
    assert_eq!(request_generation(&mut runtime, worker_before), 1);
    assert_eq!(starts.load(Ordering::SeqCst), 1);

    runtime
        .send(worker_before, CastMessage(WorkerCast::Crash))
        .expect("crash signal should deliver");
    runtime.run_until_idle();

    let worker_after = child_actor(&mut runtime, root);
    assert_ne!(worker_before, worker_after);
    assert_eq!(runtime.resolve_name(WORKER_NAME), Some(worker_after));
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
