use std::{
    collections::BTreeSet,
    num::NonZeroUsize,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use lamport::{
    Actor, ActorTurn, ConcurrentRuntime, Context, Envelope, ExitReason, LocalRuntime,
    SchedulerConfig,
};

#[derive(Debug, Clone, Copy)]
struct Start;

#[derive(Clone)]
struct SharedState {
    completed: Arc<AtomicUsize>,
    schedulers: Arc<Mutex<Vec<usize>>>,
}

impl SharedState {
    fn new() -> Self {
        Self {
            completed: Arc::new(AtomicUsize::new(0)),
            schedulers: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn record(&self, scheduler_id: usize) {
        self.completed.fetch_add(1, Ordering::SeqCst);
        self.schedulers.lock().unwrap().push(scheduler_id);
    }

    fn completed(&self) -> usize {
        self.completed.load(Ordering::SeqCst)
    }

    fn unique_schedulers(&self) -> BTreeSet<usize> {
        self.schedulers.lock().unwrap().iter().copied().collect()
    }
}

struct ExpensiveTurnActor {
    work: Duration,
    shared: SharedState,
}

impl Actor for ExpensiveTurnActor {
    fn handle<C: Context>(&mut self, envelope: Envelope, ctx: &mut C) -> ActorTurn {
        match envelope {
            Envelope::User(payload) => match payload.downcast::<Start>() {
                Ok(Start) => {
                    let scheduler_id = ctx.scheduler_id().unwrap_or(usize::MAX);
                    thread::sleep(self.work);
                    self.shared.record(scheduler_id);
                    ActorTurn::Continue
                }
                Err(payload) => ActorTurn::Stop(ExitReason::Error(format!(
                    "unexpected work payload `{}`",
                    payload.type_name()
                ))),
            },
            other => ActorTurn::Stop(ExitReason::Error(format!(
                "unexpected envelope `{other:?}`"
            ))),
        }
    }
}

struct RunSummary {
    label: &'static str,
    workers: usize,
    actors: usize,
    work: Duration,
    elapsed: Duration,
    schedulers: BTreeSet<usize>,
}

fn main() {
    let workers = std::thread::available_parallelism()
        .map_or(4, NonZeroUsize::get)
        .clamp(2, 8);
    let actors = workers * 4;
    let work = Duration::from_millis(150);

    let local = run_local(actors, work);
    let concurrent = run_concurrent(workers, actors, work);

    println!("Lamport Runtime Comparison");
    println!();
    print_summary(&local);
    println!();
    print_summary(&concurrent);
    println!();

    let speedup = local.elapsed.as_secs_f64() / concurrent.elapsed.as_secs_f64();
    let serialized = Duration::from_secs_f64(work.as_secs_f64() * actors as f64);
    let parallel_waves = actors.div_ceil(workers);
    let parallel_estimate = Duration::from_secs_f64(work.as_secs_f64() * parallel_waves as f64);

    println!("Speedup: {speedup:.2}x");
    println!("Serialized baseline: {}", format_duration(serialized));
    println!(
        "Ideal {}-worker baseline: {}",
        workers,
        format_duration(parallel_estimate)
    );
    println!();
    println!(
        "This example keeps the work inside actor turns on purpose. It shows where the Tokio-backed runtime helps: independent actors with expensive synchronous turns can progress in parallel, while LocalRuntime runs them one-at-a-time."
    );
}

fn run_local(actors: usize, work: Duration) -> RunSummary {
    let shared = SharedState::new();
    let mut runtime = LocalRuntime::default();
    let mut actor_ids = Vec::with_capacity(actors);

    for _ in 0..actors {
        actor_ids.push(
            runtime
                .spawn(ExpensiveTurnActor {
                    work,
                    shared: shared.clone(),
                })
                .expect("local spawn should succeed"),
        );
    }

    runtime.run_until_idle();

    let started = Instant::now();
    for actor in actor_ids {
        runtime
            .send(actor, Start)
            .expect("local send should succeed");
    }
    runtime.run_until_idle();
    let elapsed = started.elapsed();

    assert_eq!(shared.completed(), actors);

    RunSummary {
        label: "LocalRuntime",
        workers: 1,
        actors,
        work,
        elapsed,
        schedulers: shared.unique_schedulers(),
    }
}

fn run_concurrent(workers: usize, actors: usize, work: Duration) -> RunSummary {
    let shared = SharedState::new();
    let runtime = ConcurrentRuntime::new(SchedulerConfig {
        scheduler_count: workers,
        ..SchedulerConfig::default()
    });
    let mut actor_ids = Vec::with_capacity(actors);

    for _ in 0..actors {
        actor_ids.push(
            runtime
                .spawn(ExpensiveTurnActor {
                    work,
                    shared: shared.clone(),
                })
                .expect("concurrent spawn should succeed"),
        );
    }

    assert!(runtime.wait_for_idle(Some(Duration::from_secs(2))));

    let started = Instant::now();
    for actor in actor_ids {
        runtime
            .send(actor, Start)
            .expect("concurrent send should succeed");
    }
    assert!(runtime.wait_for_idle(Some(Duration::from_secs(10))));
    let elapsed = started.elapsed();

    assert_eq!(shared.completed(), actors);

    RunSummary {
        label: "ConcurrentRuntime",
        workers,
        actors,
        work,
        elapsed,
        schedulers: shared.unique_schedulers(),
    }
}

fn print_summary(summary: &RunSummary) {
    println!("{}", summary.label);
    println!("  actors: {}", summary.actors);
    println!("  worker threads: {}", summary.workers);
    println!("  work per actor turn: {}", format_duration(summary.work));
    println!("  elapsed: {}", format_duration(summary.elapsed));
    println!(
        "  scheduler ids observed: {}",
        format_schedulers(&summary.schedulers)
    );
}

fn format_schedulers(schedulers: &BTreeSet<usize>) -> String {
    let mut rendered = String::from("[");

    for (index, scheduler_id) in schedulers.iter().enumerate() {
        if index > 0 {
            rendered.push_str(", ");
        }
        rendered.push_str(&scheduler_id.to_string());
    }

    rendered.push(']');
    rendered
}

fn format_duration(duration: Duration) -> String {
    format!("{:.3}s", duration.as_secs_f64())
}
