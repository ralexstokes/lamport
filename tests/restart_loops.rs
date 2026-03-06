use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

mod support;

use lamport::{
    Actor, ActorId, ActorStatus, ActorTurn, ChildSpec, Context, Envelope, ExitReason,
    LifecycleEvent, LocalRuntime, Restart, Shutdown, SpawnOptions, StartChildError, Strategy,
    Supervisor, SupervisorActor, SupervisorDirective, SupervisorFlags, restart_scope,
};
use support::map_spawn_error;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Control {
    Crash,
}

struct Worker {
    id: &'static str,
}

impl Actor for Worker {
    fn handle<C: Context>(&mut self, envelope: Envelope, _ctx: &mut C) -> ActorTurn {
        match envelope {
            Envelope::User(payload)
                if payload.downcast_ref::<Control>() == Some(&Control::Crash) =>
            {
                ActorTurn::Stop(ExitReason::Error(format!("{} boom", self.id)))
            }
            _ => ActorTurn::Continue,
        }
    }
}

struct TestSupervisor {
    flags: SupervisorFlags,
    specs: Vec<ChildSpec>,
}

impl TestSupervisor {
    fn new(flags: SupervisorFlags, specs: Vec<ChildSpec>) -> Self {
        Self { flags, specs }
    }
}

impl Supervisor for TestSupervisor {
    fn flags(&self) -> SupervisorFlags {
        self.flags.clone()
    }

    fn child_specs(&self) -> &[ChildSpec] {
        &self.specs
    }

    fn start_child<C: Context>(
        &mut self,
        spec: &ChildSpec,
        ctx: &mut C,
    ) -> Result<ActorId, StartChildError> {
        ctx.spawn(Worker { id: spec.id }, SpawnOptions::default())
            .map_err(map_spawn_error)
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

fn child_specs(ids: &[&'static str]) -> Vec<ChildSpec> {
    ids.iter()
        .copied()
        .map(|id| ChildSpec {
            id,
            restart: Restart::Permanent,
            shutdown: Shutdown::default(),
            is_supervisor: false,
        })
        .collect()
}

fn child_ids(runtime: &mut LocalRuntime, supervisor: ActorId) -> BTreeMap<&'static str, ActorId> {
    runtime
        .supervisor_snapshot(supervisor)
        .expect("supervisor snapshot should exist")
        .children
        .into_iter()
        .map(|child| (child.spec.id, child.actor.expect("child should be running")))
        .collect()
}

fn wait_until_dead(runtime: &mut LocalRuntime, actor: ActorId) {
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

    panic!("actor {actor:?} did not terminate in time");
}

#[test]
fn restart_window_expires_before_the_next_failure() {
    let mut runtime = LocalRuntime::default();
    let supervisor = runtime
        .spawn(SupervisorActor::new(TestSupervisor::new(
            SupervisorFlags {
                strategy: Strategy::OneForOne,
                intensity: 1,
                period: Duration::from_millis(40),
            },
            child_specs(&["worker"]),
        )))
        .unwrap();

    runtime.run_until_idle();

    let first = child_ids(&mut runtime, supervisor)["worker"];
    runtime.send(first, Control::Crash).unwrap();
    runtime.run_until_idle();

    let second = child_ids(&mut runtime, supervisor)["worker"];
    assert_ne!(first, second);
    assert_eq!(
        runtime
            .supervisor_snapshot(supervisor)
            .unwrap()
            .active_restarts,
        1
    );

    std::thread::sleep(Duration::from_millis(70));
    assert_eq!(
        runtime
            .supervisor_snapshot(supervisor)
            .unwrap()
            .active_restarts,
        0
    );

    runtime.send(second, Control::Crash).unwrap();
    runtime.run_until_idle();

    let third = child_ids(&mut runtime, supervisor)["worker"];
    assert_ne!(second, third);
    assert_ne!(
        runtime.actor_snapshot(supervisor).unwrap().status,
        ActorStatus::Dead
    );
    assert_eq!(
        runtime
            .supervisor_snapshot(supervisor)
            .unwrap()
            .active_restarts,
        1
    );
}

#[test]
fn restart_intensity_shutdown_terminates_the_running_subtree() {
    let mut runtime = LocalRuntime::default();
    let supervisor = runtime
        .spawn(SupervisorActor::new(TestSupervisor::new(
            SupervisorFlags {
                strategy: Strategy::OneForAll,
                intensity: 1,
                period: Duration::from_secs(5),
            },
            child_specs(&["a", "b", "c"]),
        )))
        .unwrap();

    runtime.run_until_idle();

    let first_generation = child_ids(&mut runtime, supervisor);
    runtime.send(first_generation["b"], Control::Crash).unwrap();
    runtime.run_until_idle();

    let second_generation = child_ids(&mut runtime, supervisor);
    assert_ne!(first_generation["a"], second_generation["a"]);
    assert_ne!(first_generation["b"], second_generation["b"]);
    assert_ne!(first_generation["c"], second_generation["c"]);

    let baseline_events = runtime.lifecycle_events().len();
    runtime
        .send(second_generation["b"], Control::Crash)
        .unwrap();
    runtime.run_until_idle();
    wait_until_dead(&mut runtime, supervisor);

    let supervisor_snapshot = runtime.actor_snapshot(supervisor).unwrap();
    assert_eq!(supervisor_snapshot.status, ActorStatus::Dead);
    assert_eq!(
        supervisor_snapshot.metrics.last_exit,
        Some(ExitReason::Error(
            "supervisor restart intensity exceeded".into()
        ))
    );

    for actor in second_generation.values().copied() {
        assert_eq!(
            runtime.actor_snapshot(actor).unwrap().status,
            ActorStatus::Dead
        );
    }

    let events = runtime.lifecycle_events();
    let requested: Vec<_> = events[baseline_events..]
        .iter()
        .filter_map(|event| match event {
            LifecycleEvent::Shutdown {
                phase: lamport::ShutdownPhase::Requested,
                actor,
                ..
            } => Some(*actor),
            _ => None,
        })
        .collect();
    assert_eq!(
        requested,
        vec![second_generation["c"], second_generation["a"],]
    );

    assert_eq!(
        runtime
            .actor_snapshot(second_generation["b"])
            .unwrap()
            .status,
        ActorStatus::Dead
    );
}
