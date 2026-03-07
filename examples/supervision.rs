use std::time::{Duration, Instant};

use lamport::{
    Actor, ActorId, ActorStatus, ActorTurn, Application, CallOutcome, ChildSpec, Context,
    DownMessage, Envelope, ExitReason, GenServer, Restart, RuntimeInfo, ServerOutcome, Shutdown,
    SpawnOptions, StartChildError, Strategy, Supervisor, SupervisorDirective, SupervisorFlags,
    TimerToken, boot_local_application, restart_scope,
};

const WORKER_CHILD: &str = "worker";
const ROOT_NAME: &str = "demo.root";
const WORKER_NAME: &str = "demo.worker";
const RESTART_INTENSITY: u32 = 2;
const RESTART_PERIOD: Duration = Duration::from_millis(250);

#[derive(Debug)]
struct WorkerState {
    crash_token: TimerToken,
}

struct FlakyWorker;

impl GenServer for FlakyWorker {
    type State = WorkerState;
    type Call = ();
    type Cast = ();
    type Reply = ();
    type Info = RuntimeInfo;

    fn init<C: Context>(&mut self, ctx: &mut C) -> Result<Self::State, ExitReason> {
        let crash_token = TimerToken::next();
        ctx.schedule_after(Duration::from_millis(40), crash_token)
            .map_err(|error| ExitReason::Error(format!("schedule crash failed: {error:?}")))?;

        println!("worker {} started", ctx.actor_id());
        Ok(WorkerState { crash_token })
    }

    fn handle_call<C: Context>(
        &mut self,
        _state: &mut Self::State,
        _from: lamport::ReplyToken,
        _message: Self::Call,
        _ctx: &mut C,
    ) -> CallOutcome<Self::Reply> {
        CallOutcome::Reply(())
    }

    fn handle_cast<C: Context>(
        &mut self,
        _state: &mut Self::State,
        _message: Self::Cast,
        _ctx: &mut C,
    ) -> ServerOutcome {
        ServerOutcome::Continue
    }

    fn handle_info<C: Context>(
        &mut self,
        state: &mut Self::State,
        message: Self::Info,
        ctx: &mut C,
    ) -> ServerOutcome {
        match message {
            RuntimeInfo::Timer(timer) if timer.token == state.crash_token => {
                println!("worker {} simulating a crash", ctx.actor_id());
                ServerOutcome::Stop(ExitReason::Error("simulated failure".into()))
            }
            _ => ServerOutcome::Continue,
        }
    }

    fn terminate<C: Context>(&mut self, _state: &mut Self::State, reason: ExitReason, ctx: &mut C) {
        println!("worker {} terminated with {reason}", ctx.actor_id());
    }
}

struct MonitorProbe {
    target_name: &'static str,
    retry_token: TimerToken,
    current_target: Option<ActorId>,
    current_monitor: Option<lamport::Ref>,
}

impl MonitorProbe {
    fn new(target_name: &'static str) -> Self {
        Self {
            target_name,
            retry_token: TimerToken::next(),
            current_target: None,
            current_monitor: None,
        }
    }

    fn arm<C: Context>(&self, ctx: &mut C) -> Result<(), ExitReason> {
        ctx.schedule_after(Duration::from_millis(15), self.retry_token)
            .map_err(|error| ExitReason::Error(format!("schedule retry failed: {error:?}")))
    }
}

impl Actor for MonitorProbe {
    fn init<C: Context>(&mut self, ctx: &mut C) -> Result<(), ExitReason> {
        self.arm(ctx)
    }

    fn handle<C: Context>(&mut self, envelope: Envelope, ctx: &mut C) -> ActorTurn {
        match envelope {
            Envelope::Timer(timer) if timer.token == self.retry_token => {
                if self.current_target.is_none() {
                    if let Some(target) = ctx.whereis(self.target_name) {
                        match ctx.monitor(target) {
                            Ok(reference) => {
                                self.current_target = Some(target);
                                self.current_monitor = Some(reference);
                                println!("monitor armed for worker {target}");
                            }
                            Err(error) => {
                                eprintln!("monitor failed: {error:?}");
                                let _ = self.arm(ctx);
                            }
                        }
                    } else {
                        let _ = self.arm(ctx);
                    }
                }

                ActorTurn::Continue
            }
            Envelope::Down(DownMessage {
                reference,
                actor,
                reason,
            }) if self.current_monitor == Some(reference) => {
                println!("monitor observed worker {actor} exit with {reason}");
                self.current_target = None;
                self.current_monitor = None;
                let _ = self.arm(ctx);
                ActorTurn::Continue
            }
            _ => ActorTurn::Continue,
        }
    }
}

struct DemoSupervisor {
    flags: SupervisorFlags,
    specs: Vec<ChildSpec>,
}

impl DemoSupervisor {
    fn new() -> Self {
        Self {
            flags: SupervisorFlags {
                strategy: Strategy::OneForOne,
                intensity: RESTART_INTENSITY,
                period: RESTART_PERIOD,
            },
            specs: vec![ChildSpec {
                id: WORKER_CHILD,
                restart: Restart::Permanent,
                shutdown: Shutdown::default(),
                is_supervisor: false,
            }],
        }
    }
}

impl Supervisor for DemoSupervisor {
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
                    FlakyWorker,
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
        actor: ActorId,
        reason: ExitReason,
        _ctx: &mut C,
    ) -> SupervisorDirective {
        println!(
            "supervisor saw child {} ({actor}) exit with {reason}",
            spec.id
        );

        if spec.should_restart(&reason) {
            SupervisorDirective::Restart(restart_scope(self.flags.strategy, &self.specs, spec.id))
        } else {
            SupervisorDirective::Ignore
        }
    }
}

struct RecoveryApplication;

impl Application for RecoveryApplication {
    type RootSupervisor = DemoSupervisor;

    fn name(&self) -> &'static str {
        "recovery-demo"
    }

    fn root_options(&self) -> SpawnOptions {
        SpawnOptions {
            registered_name: Some(ROOT_NAME.into()),
            ..SpawnOptions::default()
        }
    }

    fn root_supervisor(self) -> Self::RootSupervisor {
        DemoSupervisor::new()
    }
}

fn wait_until_dead(runtime: &mut lamport::LocalRuntime, actor: ActorId) {
    let deadline = Instant::now() + Duration::from_secs(2);

    while Instant::now() < deadline {
        if runtime
            .actor_snapshot(actor)
            .is_some_and(|snapshot| snapshot.status == ActorStatus::Dead)
        {
            return;
        }

        let _ = runtime.block_on_next(Some(Duration::from_millis(50)));
        runtime.run_until_idle();
    }

    panic!("actor {actor} did not terminate in time");
}

fn main() {
    let (mut runtime, app) =
        boot_local_application(RecoveryApplication, Default::default()).expect("boot app");
    let root = app.root_supervisor();

    println!(
        "restart window: max {} restarts within {:?}",
        RESTART_INTENSITY, RESTART_PERIOD
    );

    runtime
        .spawn(MonitorProbe::new(WORKER_NAME))
        .expect("spawn monitor");
    runtime.run_until_idle();

    wait_until_dead(&mut runtime, root);

    let root_snapshot = runtime
        .actor_snapshot(root)
        .expect("root snapshot should exist");

    println!("application {} root supervisor {}", app.name(), root);
    println!("root status: {:?}", root_snapshot.status);
    println!("root last exit: {:?}", root_snapshot.metrics.last_exit);
    println!("registered root: {:?}", runtime.resolve_name(ROOT_NAME));
    println!("registered worker: {:?}", runtime.resolve_name(WORKER_NAME));
    println!("lifecycle events: {}", runtime.lifecycle_events().len());

    if let Some(report) = runtime.crash_reports().last() {
        println!("last crash report: {report}");
    }
}
