use std::{
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use lamport::{
    Actor, ActorId, ActorStatus, ActorTurn, ConcurrentRuntime, Context, DownMessage, Envelope,
    ExitReason, SchedulerConfig, SpawnOptions,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    target: ActorId,
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

struct TrappingLinkActor {
    target: ActorId,
    seen: Arc<Mutex<Vec<(ActorId, ActorId, ExitReason)>>>,
}

impl Actor for TrappingLinkActor {
    fn init<C: Context>(&mut self, ctx: &mut C) -> Result<(), ExitReason> {
        ctx.link(self.target)
            .map_err(|error| ExitReason::Error(format!("link failed: {error:?}")))?;
        Ok(())
    }

    fn handle<C: Context>(&mut self, envelope: Envelope, _ctx: &mut C) -> ActorTurn {
        if let Envelope::Exit(signal) = envelope {
            self.seen
                .lock()
                .unwrap()
                .push((self.target, signal.from, signal.reason));
        }

        ActorTurn::Continue
    }
}

struct MonitoringActor {
    target: ActorId,
    seen: Arc<Mutex<Vec<(ActorId, ActorId, ExitReason)>>>,
}

impl Actor for MonitoringActor {
    fn init<C: Context>(&mut self, ctx: &mut C) -> Result<(), ExitReason> {
        ctx.monitor(self.target)
            .map_err(|error| ExitReason::Error(format!("monitor failed: {error:?}")))?;
        Ok(())
    }

    fn handle<C: Context>(&mut self, envelope: Envelope, _ctx: &mut C) -> ActorTurn {
        if let Envelope::Down(DownMessage { actor, reason, .. }) = envelope {
            self.seen.lock().unwrap().push((self.target, actor, reason));
        }

        ActorTurn::Continue
    }
}

#[test]
fn concurrent_runtime_propagates_links_and_monitors_under_load() {
    const TARGETS: usize = 8;
    const LINKS_PER_TARGET: usize = 6;
    const TRAPPERS_PER_TARGET: usize = 4;
    const MONITORS_PER_TARGET: usize = 6;

    let runtime = ConcurrentRuntime::new(SchedulerConfig {
        scheduler_count: 4,
        ..SchedulerConfig::default()
    });
    let trapped = Arc::new(Mutex::new(Vec::new()));
    let down = Arc::new(Mutex::new(Vec::new()));

    let mut targets = Vec::new();
    let mut linked = Vec::new();
    let mut trappers = Vec::new();
    let mut monitors = Vec::new();

    for _ in 0..TARGETS {
        let target = runtime.spawn(CrashActor).unwrap();
        targets.push(target);

        for _ in 0..LINKS_PER_TARGET {
            linked.push(runtime.spawn(LinkedActor { target }).unwrap());
        }

        for _ in 0..TRAPPERS_PER_TARGET {
            trappers.push(
                runtime
                    .spawn_with_options(
                        TrappingLinkActor {
                            target,
                            seen: Arc::clone(&trapped),
                        },
                        SpawnOptions {
                            trap_exit: true,
                            ..SpawnOptions::default()
                        },
                    )
                    .unwrap(),
            );
        }

        for _ in 0..MONITORS_PER_TARGET {
            monitors.push(
                runtime
                    .spawn(MonitoringActor {
                        target,
                        seen: Arc::clone(&down),
                    })
                    .unwrap(),
            );
        }
    }

    assert!(runtime.wait_for_idle(Some(Duration::from_secs(2))));

    for target in &targets {
        runtime.send(*target, Control::Crash).unwrap();
    }

    let expected_trapped = TARGETS * TRAPPERS_PER_TARGET;
    let expected_down = TARGETS * MONITORS_PER_TARGET;
    let deadline = Instant::now() + Duration::from_secs(5);

    while Instant::now() < deadline {
        if trapped.lock().unwrap().len() == expected_trapped
            && down.lock().unwrap().len() == expected_down
            && linked.iter().all(|actor| {
                runtime
                    .actor_snapshot(*actor)
                    .is_some_and(|snapshot| snapshot.status == ActorStatus::Dead)
            })
        {
            break;
        }

        let _ = runtime.wait_for_idle(Some(Duration::from_millis(50)));
        thread::sleep(Duration::from_millis(10));
    }

    let trapped = trapped.lock().unwrap().clone();
    let down = down.lock().unwrap().clone();

    assert_eq!(trapped.len(), expected_trapped);
    assert_eq!(down.len(), expected_down);
    assert!(trapped.iter().all(|(expected, actual, reason)| {
        expected == actual && *reason == ExitReason::Error("boom".into())
    }));
    assert!(down.iter().all(|(expected, actual, reason)| {
        expected == actual && *reason == ExitReason::Error("boom".into())
    }));

    for actor in targets {
        assert_eq!(
            runtime.actor_snapshot(actor).unwrap().status,
            ActorStatus::Dead
        );
    }

    for actor in linked {
        assert_eq!(
            runtime.actor_snapshot(actor).unwrap().status,
            ActorStatus::Dead
        );
    }

    for actor in trappers {
        assert_eq!(
            runtime.actor_snapshot(actor).unwrap().status,
            ActorStatus::Waiting
        );
    }

    for actor in monitors {
        assert_eq!(
            runtime.actor_snapshot(actor).unwrap().status,
            ActorStatus::Waiting
        );
    }
}
