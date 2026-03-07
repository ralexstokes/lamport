use std::collections::BTreeMap;

use lamport::{
    Actor, ActorId, ActorTurn, Context, Envelope, ExitReason, LifecycleEvent, LocalRuntime,
    SpawnOptions,
    observability::{
        ActorTreeNode, EventCursor, RuntimeEvent, RuntimeEventKind, RuntimeIntrospection,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Control {
    Crash,
}

struct IdleActor;

impl Actor for IdleActor {
    fn handle<C: Context>(&mut self, _envelope: Envelope, _ctx: &mut C) -> ActorTurn {
        ActorTurn::Continue
    }
}

struct WorkerActor;

impl Actor for WorkerActor {
    fn handle<C: Context>(&mut self, envelope: Envelope, _ctx: &mut C) -> ActorTurn {
        match envelope {
            Envelope::User(payload) => match payload.downcast::<Control>() {
                Ok(Control::Crash) => ActorTurn::Stop(ExitReason::Error("boom".into())),
                Err(payload) => ActorTurn::Stop(ExitReason::Error(format!(
                    "unexpected worker message `{}`",
                    payload.type_name()
                ))),
            },
            other => ActorTurn::Stop(ExitReason::Error(format!(
                "unexpected worker envelope `{other:?}`"
            ))),
        }
    }
}

struct MonitorActor {
    target: ActorId,
}

impl Actor for MonitorActor {
    fn init<C: Context>(&mut self, ctx: &mut C) -> Result<(), ExitReason> {
        ctx.monitor(self.target)
            .map_err(|error| ExitReason::Error(format!("monitor failed: {error:?}")))?;
        Ok(())
    }

    fn handle<C: Context>(&mut self, envelope: Envelope, _ctx: &mut C) -> ActorTurn {
        match envelope {
            Envelope::Down(message) => {
                println!(
                    "observer received DOWN for {}: {}",
                    message.actor, message.reason
                );
                ActorTurn::Continue
            }
            _ => ActorTurn::Continue,
        }
    }
}

fn main() {
    // Install a tracing subscriber in your application if you want the
    // runtime's spans and lifecycle events exported to stdout, OTel, or another
    // backend. This example sticks to the retained observability API.
    let mut runtime = LocalRuntime::default();

    let root = runtime
        .spawn_with_options(
            IdleActor,
            SpawnOptions {
                registered_name: Some("root".into()),
                ..SpawnOptions::default()
            },
        )
        .expect("root spawn should succeed");
    let worker = runtime
        .spawn_with_options(
            WorkerActor,
            SpawnOptions {
                registered_name: Some("worker".into()),
                parent: Some(root),
                ancestors: vec![root],
                ..SpawnOptions::default()
            },
        )
        .expect("worker spawn should succeed");
    runtime
        .spawn_with_options(
            MonitorActor { target: worker },
            SpawnOptions {
                registered_name: Some("observer".into()),
                parent: Some(root),
                ancestors: vec![root],
                ..SpawnOptions::default()
            },
        )
        .expect("observer spawn should succeed");

    runtime.run_until_idle();

    println!("Initial runtime snapshot");
    print_introspection(&runtime.introspection());

    let mut startup_cursor = EventCursor::from_start();
    println!("\nStartup event log");
    print_events(&runtime.events_since(&mut startup_cursor));

    let mut tail_cursor = runtime.event_cursor();
    runtime
        .send(worker, Control::Crash)
        .expect("crash command should enqueue");
    runtime.run_until_idle();

    println!("\nEvents after crashing the worker");
    print_events(&runtime.events_since(&mut tail_cursor));

    println!("\nCrash reports");
    for report in runtime.crash_reports() {
        println!("  {report}");
    }

    println!("\nRuntime snapshot after crash");
    print_introspection(&runtime.introspection());

    println!("\nPrometheus metrics");
    println!("{}", runtime.export_metrics_prometheus());
}

fn print_introspection(introspection: &RuntimeIntrospection) {
    println!("  live actors: {}", introspection.metrics.live_actors);
    println!(
        "  scheduler utilization: {:.3}",
        introspection.metrics.scheduler_metrics.utilization
    );
    println!(
        "  total mailbox length: {}",
        introspection.metrics.total_mailbox_len
    );
    println!("  actor tree:");

    let nodes_by_id: BTreeMap<_, _> = introspection
        .actor_tree
        .nodes
        .iter()
        .map(|node| (node.actor.id, node))
        .collect();

    for root in &introspection.actor_tree.roots {
        print_tree(*root, 0, &nodes_by_id);
    }
}

fn print_tree(actor: ActorId, depth: usize, nodes_by_id: &BTreeMap<ActorId, &ActorTreeNode>) {
    let Some(node) = nodes_by_id.get(&actor) else {
        return;
    };

    let indent = "  ".repeat(depth + 2);
    let registered_name = node.actor.registered_name.as_deref().unwrap_or("-");
    println!(
        "{indent}- {} `{}` registered={} mailbox={} links={} monitors_in={} monitors_out={}",
        node.actor.id,
        node.actor.name,
        registered_name,
        node.actor.metrics.mailbox_len,
        node.actor.links.len(),
        node.actor.monitors_in.len(),
        node.actor.monitors_out.len(),
    );

    for child in &node.children {
        print_tree(*child, depth + 1, nodes_by_id);
    }
}

fn print_events(events: &[RuntimeEvent]) {
    for event in events {
        match &event.kind {
            RuntimeEventKind::Lifecycle(LifecycleEvent::Spawn {
                actor,
                name,
                parent,
                registered_name,
                ..
            }) => {
                let parent = parent.map(|parent| parent.to_string());
                println!(
                    "  #{} spawn {} `{}` parent={} registered={}",
                    event.sequence,
                    actor,
                    name,
                    parent.as_deref().unwrap_or("-"),
                    registered_name.as_deref().unwrap_or("-")
                );
            }
            RuntimeEventKind::Lifecycle(LifecycleEvent::Exit {
                actor,
                name,
                reason,
                ..
            }) => {
                println!(
                    "  #{} exit {} `{}` reason={}",
                    event.sequence, actor, name, reason
                );
            }
            RuntimeEventKind::Lifecycle(LifecycleEvent::Down {
                watcher,
                actor,
                reference,
                reason,
            }) => {
                println!(
                    "  #{} down watcher={} actor={} ref={} reason={}",
                    event.sequence,
                    watcher,
                    actor,
                    reference.get(),
                    reason
                );
            }
            RuntimeEventKind::Lifecycle(LifecycleEvent::Restart {
                supervisor,
                child_id,
                old_actor,
                new_actor,
            }) => {
                println!(
                    "  #{} restart supervisor={} child={} old={old_actor:?} new={}",
                    event.sequence, supervisor, child_id, new_actor
                );
            }
            RuntimeEventKind::Lifecycle(LifecycleEvent::Shutdown {
                requester,
                actor,
                phase,
                reason,
                ..
            }) => {
                println!(
                    "  #{} shutdown requester={} actor={} phase={phase:?} reason={reason:?}",
                    event.sequence, requester, actor
                );
            }
            RuntimeEventKind::Crash(report) => {
                println!("  #{} crash {}", event.sequence, report);
            }
        }
    }
}
