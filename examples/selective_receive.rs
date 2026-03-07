use std::sync::{Arc, Mutex};

use lamport::{
    Actor, ActorId, ActorTurn, Context, Envelope, ExitReason, LocalRuntime, ReceivedEnvelope,
    SpawnOptions,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CrashNow;

struct ReplyActor;

impl Actor for ReplyActor {
    fn handle<C: Context>(&mut self, envelope: Envelope, ctx: &mut C) -> ActorTurn {
        match envelope {
            Envelope::Request { token, message } => {
                let value = match message.downcast::<u32>() {
                    Ok(value) => value,
                    Err(payload) => {
                        return ActorTurn::Stop(ExitReason::Error(format!(
                            "unexpected request payload `{}`",
                            payload.type_name()
                        )));
                    }
                };

                if let Err(error) = ctx.reply(token, value + 1) {
                    return ActorTurn::Stop(ExitReason::Error(format!("reply failed: {error:?}")));
                }

                ActorTurn::Continue
            }
            other => ActorTurn::Stop(ExitReason::Error(format!(
                "unexpected reply-actor envelope `{other:?}`"
            ))),
        }
    }
}

struct CrashActor;

impl Actor for CrashActor {
    fn handle<C: Context>(&mut self, envelope: Envelope, _ctx: &mut C) -> ActorTurn {
        match envelope {
            Envelope::User(payload) => match payload.downcast::<CrashNow>() {
                Ok(CrashNow) => ActorTurn::Stop(ExitReason::Error("intentional crash".into())),
                Err(payload) => ActorTurn::Stop(ExitReason::Error(format!(
                    "unexpected crash payload `{}`",
                    payload.type_name()
                ))),
            },
            other => ActorTurn::Stop(ExitReason::Error(format!(
                "unexpected crash-actor envelope `{other:?}`"
            ))),
        }
    }
}

struct SelectiveClient {
    reply_actor: ActorId,
    crash_actor: ActorId,
    seen: Arc<Mutex<Vec<String>>>,
    pending: Option<lamport::PendingCall>,
}

impl Actor for SelectiveClient {
    fn init<C: Context>(&mut self, ctx: &mut C) -> Result<(), ExitReason> {
        ctx.link(self.crash_actor)
            .map_err(|error| ExitReason::Error(format!("link failed: {error:?}")))?;

        ctx.send(ctx.actor_id(), 7_u32)
            .map_err(|error| ExitReason::Error(format!("self-send failed: {error:?}")))?;

        self.pending = Some(
            ctx.ask(self.reply_actor, 41_u32, None)
                .map_err(|error| ExitReason::Error(format!("ask failed: {error:?}")))?,
        );

        ctx.send(self.crash_actor, CrashNow)
            .map_err(|error| ExitReason::Error(format!("crash send failed: {error:?}")))?;

        Ok(())
    }

    fn select_envelope<C: Context>(&mut self, ctx: &mut C) -> Option<ReceivedEnvelope> {
        if let Some(pending) = self.pending {
            return ctx.receive_selective_after(pending.mailbox_watermark, |envelope| {
                matches!(
                    envelope,
                    Envelope::Reply { reference, .. } if *reference == pending.reference
                ) || matches!(
                    envelope,
                    Envelope::CallTimeout(timeout) if timeout.reference == pending.reference
                )
            });
        }

        ctx.receive_next()
    }

    fn handle<C: Context>(&mut self, envelope: Envelope, _ctx: &mut C) -> ActorTurn {
        let label = match envelope {
            Envelope::Exit(signal) => format!("linked exit observed first: {}", signal.reason),
            Envelope::Reply { reference, message } => {
                assert_eq!(
                    Some(reference),
                    self.pending.map(|pending| pending.reference)
                );
                self.pending = None;
                format!(
                    "reply after watermark: {}",
                    message.downcast::<u32>().ok().unwrap()
                )
            }
            Envelope::User(payload) => {
                format!(
                    "older mailbox message drained later: {}",
                    payload.downcast::<u32>().ok().unwrap()
                )
            }
            other => format!("unexpected envelope: {other:?}"),
        };

        self.seen.lock().unwrap().push(label);
        ActorTurn::Continue
    }
}

fn run() -> Result<(), String> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut runtime = LocalRuntime::default();

    let reply_actor = runtime
        .spawn(ReplyActor)
        .map_err(|error| format!("spawn reply actor failed: {error:?}"))?;
    let crash_actor = runtime
        .spawn(CrashActor)
        .map_err(|error| format!("spawn crash actor failed: {error:?}"))?;
    let client = runtime
        .spawn_with_options(
            SelectiveClient {
                reply_actor,
                crash_actor,
                seen: Arc::clone(&seen),
                pending: None,
            },
            SpawnOptions {
                trap_exit: true,
                ..SpawnOptions::default()
            },
        )
        .map_err(|error| format!("spawn client failed: {error:?}"))?;

    runtime.run_until_idle();

    println!("Actor-selected receive example");
    println!();
    for line in seen.lock().unwrap().iter() {
        println!("- {line}");
    }
    println!();
    println!("client snapshot: {:?}", runtime.actor_snapshot(client));
    println!("crash snapshot: {:?}", runtime.actor_snapshot(crash_actor));

    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
