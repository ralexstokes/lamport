use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use lamport::{
    Actor, ActorTurn, Context, Envelope, ExitReason, LocalRuntime, SchedulerConfig, SpawnOptions,
    TimerToken, mailbox::Mailbox,
};

struct SinkActor;

impl Actor for SinkActor {
    fn handle<C: Context>(&mut self, _envelope: Envelope, _ctx: &mut C) -> ActorTurn {
        ActorTurn::Continue
    }
}

struct TimerFloodActor {
    expected: usize,
    seen: Arc<AtomicUsize>,
    tokens: Vec<TimerToken>,
}

impl Actor for TimerFloodActor {
    fn init<C: Context>(&mut self, ctx: &mut C) -> Result<(), ExitReason> {
        for token in &self.tokens {
            ctx.schedule_after(Duration::ZERO, *token)
                .map_err(|error| ExitReason::Error(format!("timer schedule failed: {error:?}")))?;
        }
        Ok(())
    }

    fn handle<C: Context>(&mut self, envelope: Envelope, _ctx: &mut C) -> ActorTurn {
        if matches!(envelope, Envelope::Timer(_)) {
            let delivered = self.seen.fetch_add(1, Ordering::SeqCst) + 1;
            if delivered == self.expected {
                return ActorTurn::Stop(ExitReason::Normal);
            }
        }

        ActorTurn::Continue
    }
}

fn envelope_u64(envelope: Envelope) -> u64 {
    match envelope {
        Envelope::User(payload) => payload.downcast::<u64>().ok().unwrap(),
        other => panic!("unexpected envelope: {other:?}"),
    }
}

fn bench(name: &str, units: usize, mut run: impl FnMut() -> Duration) {
    let elapsed = run();
    let nanos_per_unit = elapsed.as_secs_f64() * 1_000_000_000.0 / units as f64;
    let units_per_second = units as f64 / elapsed.as_secs_f64().max(f64::EPSILON);

    println!(
        "{name:20} total={elapsed:?} units={units} ns_per_unit={nanos_per_unit:.1} units_per_sec={units_per_second:.1}"
    );
}

fn actor_spawn_bench(iterations: usize) -> Duration {
    let mut runtime = LocalRuntime::new(SchedulerConfig {
        max_actors: iterations + 16,
        ..SchedulerConfig::default()
    });
    let started = Instant::now();

    for _ in 0..iterations {
        runtime.spawn(SinkActor).unwrap();
    }

    started.elapsed()
}

fn send_latency_bench(iterations: usize) -> Duration {
    let mut runtime = LocalRuntime::default();
    let actor = runtime
        .spawn_with_options(
            SinkActor,
            SpawnOptions {
                mailbox_capacity: Some(iterations + 64),
                ..SpawnOptions::default()
            },
        )
        .unwrap();

    let started = Instant::now();
    for value in 0..iterations {
        runtime.send(actor, value as u64).unwrap();
    }
    let elapsed = started.elapsed();
    runtime.run_until_idle();
    elapsed
}

fn mailbox_scan_bench(iterations: usize, depth: usize) -> Duration {
    let started = Instant::now();

    for round in 0..iterations {
        let mut mailbox = Mailbox::new();
        for index in 0..depth {
            mailbox.push(Envelope::user(index as u64));
        }
        let target = (depth + round) as u64;
        mailbox.push(Envelope::user(target));

        let matched = mailbox
            .selective_receive(|envelope| {
                matches!(envelope, Envelope::User(payload) if payload.downcast_ref::<u64>() == Some(&target))
            })
            .map(envelope_u64);
        assert_eq!(matched, Some(target));
    }

    started.elapsed()
}

fn timer_throughput_bench(timer_count: usize) -> Duration {
    let seen = Arc::new(AtomicUsize::new(0));
    let mut runtime = LocalRuntime::default();
    let tokens: Vec<_> = (0..timer_count).map(|_| TimerToken::next()).collect();

    let started = Instant::now();
    runtime
        .spawn_with_options(
            TimerFloodActor {
                expected: timer_count,
                seen: Arc::clone(&seen),
                tokens,
            },
            SpawnOptions {
                mailbox_capacity: Some(timer_count + 64),
                ..SpawnOptions::default()
            },
        )
        .unwrap();
    runtime.run_until_idle();

    while seen.load(Ordering::SeqCst) < timer_count {
        assert!(runtime.block_on_next(Some(Duration::from_secs(1))));
        runtime.run_until_idle();
    }

    started.elapsed()
}

fn main() {
    let full = std::env::args().any(|arg| arg == "--full")
        || std::env::var_os("LAMPORT_BENCH_FULL").is_some();
    let quick = !full;

    let actor_spawns = if quick { 5_000 } else { 50_000 };
    let sends = if quick { 20_000 } else { 200_000 };
    let mailbox_scans = if quick { 1_000 } else { 10_000 };
    let mailbox_depth = if quick { 256 } else { 4_096 };
    let timers = if quick { 2_000 } else { 20_000 };

    bench("actor_spawn", actor_spawns, || {
        actor_spawn_bench(actor_spawns)
    });
    bench("send_latency", sends, || send_latency_bench(sends));
    bench("mailbox_scan", mailbox_scans, || {
        mailbox_scan_bench(mailbox_scans, mailbox_depth)
    });
    bench("timer_throughput", timers, || {
        timer_throughput_bench(timers)
    });
}
