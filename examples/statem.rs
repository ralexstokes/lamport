use std::time::Duration;

use lamport::{
    Actor, ActorTurn, CastMessage, Context, Envelope, ExitReason, GenStatem, LocalRuntime,
    ReplyToken, SchedulerConfig, StatemCallOutcome, StatemOutcome, TimerToken,
    behaviour::RuntimeInfo,
};

#[derive(Debug, Clone, PartialEq, Eq)]
enum LightState {
    Red,
    Green,
    Yellow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LightCall {
    Snapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LightCast {
    Force(LightState),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LightReply {
    Snapshot { state: LightState, cycles: usize },
}

#[derive(Debug)]
struct LightData {
    timer: TimerToken,
    interval: Duration,
    cycles: usize,
}

struct TrafficLight {
    interval: Duration,
}

impl TrafficLight {
    fn new(interval: Duration) -> Self {
        Self { interval }
    }
}

impl GenStatem for TrafficLight {
    type State = LightState;
    type Data = LightData;
    type Call = LightCall;
    type Cast = LightCast;
    type Reply = LightReply;
    type Info = RuntimeInfo;

    fn init<C: Context>(&mut self, ctx: &mut C) -> Result<(Self::State, Self::Data), ExitReason> {
        let timer = TimerToken::next();
        ctx.schedule_after(self.interval, timer)
            .map_err(|error| ExitReason::Error(format!("schedule tick failed: {error:?}")))?;

        Ok((
            LightState::Red,
            LightData {
                timer,
                interval: self.interval,
                cycles: 0,
            },
        ))
    }

    fn handle_call<C: Context>(
        &mut self,
        state: &mut Self::State,
        data: &mut Self::Data,
        _from: ReplyToken,
        message: Self::Call,
        _ctx: &mut C,
    ) -> StatemCallOutcome<Self::State, Self::Reply> {
        match message {
            LightCall::Snapshot => StatemCallOutcome::Reply(LightReply::Snapshot {
                state: state.clone(),
                cycles: data.cycles,
            }),
        }
    }

    fn handle_cast<C: Context>(
        &mut self,
        _state: &mut Self::State,
        _data: &mut Self::Data,
        message: Self::Cast,
        _ctx: &mut C,
    ) -> StatemOutcome<Self::State> {
        match message {
            LightCast::Force(next) => StatemOutcome::Transition(next),
        }
    }

    fn handle_info<C: Context>(
        &mut self,
        state: &mut Self::State,
        data: &mut Self::Data,
        message: Self::Info,
        ctx: &mut C,
    ) -> StatemOutcome<Self::State> {
        match message {
            RuntimeInfo::Timer(timer) if timer.token == data.timer => {
                data.cycles += 1;
                match ctx.schedule_after(data.interval, data.timer) {
                    Ok(()) => StatemOutcome::Transition(next_light(state)),
                    Err(error) => StatemOutcome::Stop(ExitReason::Error(format!(
                        "reschedule tick failed: {error:?}"
                    ))),
                }
            }
            RuntimeInfo::Timer(_) => StatemOutcome::Continue,
            _ => StatemOutcome::Continue,
        }
    }
}

fn next_light(current: &LightState) -> LightState {
    match current {
        LightState::Red => LightState::Green,
        LightState::Green => LightState::Yellow,
        LightState::Yellow => LightState::Red,
    }
}

struct Inspector {
    light: lamport::ActorId,
    query_token: TimerToken,
    polls_left: usize,
}

impl Inspector {
    fn new(light: lamport::ActorId) -> Self {
        Self {
            light,
            query_token: TimerToken::next(),
            polls_left: 3,
        }
    }

    fn schedule_next<C: Context>(&self, ctx: &mut C) -> Result<(), ExitReason> {
        ctx.schedule_after(Duration::from_millis(30), self.query_token)
            .map_err(|error| ExitReason::Error(format!("schedule query failed: {error:?}")))
    }
}

impl Actor for Inspector {
    fn init<C: Context>(&mut self, ctx: &mut C) -> Result<(), ExitReason> {
        ctx.ask(self.light, LightCall::Snapshot, None)
            .map_err(|error| ExitReason::Error(format!("initial ask failed: {error:?}")))?;
        self.schedule_next(ctx)
    }

    fn handle<C: Context>(&mut self, envelope: Envelope, ctx: &mut C) -> ActorTurn {
        match envelope {
            Envelope::Reply { message, .. } => {
                let reply = match message.downcast::<LightReply>() {
                    Ok(reply) => reply,
                    Err(payload) => {
                        return ActorTurn::Stop(ExitReason::Error(format!(
                            "unexpected reply payload `{}`",
                            payload.type_name()
                        )));
                    }
                };

                println!("reply: {reply:?}");
                ActorTurn::Continue
            }
            Envelope::Timer(timer) if timer.token == self.query_token => {
                if self.polls_left == 0 {
                    return ActorTurn::Stop(ExitReason::Normal);
                }

                if self.polls_left == 2
                    && let Err(error) = ctx.send(
                        self.light,
                        CastMessage(LightCast::Force(LightState::Yellow)),
                    )
                {
                    return ActorTurn::Stop(ExitReason::Error(format!(
                        "send force transition failed: {error:?}"
                    )));
                }

                if let Err(error) = ctx.ask(self.light, LightCall::Snapshot, None) {
                    return ActorTurn::Stop(ExitReason::Error(format!(
                        "ask light failed: {error:?}"
                    )));
                }
                self.polls_left -= 1;
                if let Err(reason) = self.schedule_next(ctx) {
                    return ActorTurn::Stop(reason);
                }
                ActorTurn::Continue
            }
            _ => ActorTurn::Continue,
        }
    }
}

fn run() -> Result<(), String> {
    let mut runtime = LocalRuntime::new(SchedulerConfig::default());
    let light = runtime
        .spawn_gen_statem(TrafficLight::new(Duration::from_millis(20)))
        .map_err(|error| format!("spawn light failed: {error:?}"))?;
    runtime
        .spawn(Inspector::new(light))
        .map_err(|error| format!("spawn inspector failed: {error:?}"))?;

    for _ in 0..20 {
        if !runtime.block_on_next(Some(Duration::from_millis(40))) {
            break;
        }
    }

    println!("final snapshot: {:?}", runtime.actor_snapshot(light));

    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
