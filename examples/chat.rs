use std::{
    collections::{BTreeMap, VecDeque},
    time::Duration,
};

use lamport::{
    Actor, ActorId, ActorTurn, CallOutcome, CastMessage, ChildSpec, Context, Envelope, ExitReason,
    GenServer, LocalRuntime, ReplyToken, Restart, RuntimeInfo, ServerOutcome, Shutdown,
    StartChildError, Strategy, Supervisor, SupervisorDirective, SupervisorFlags, TimerToken,
};

const ROOM_CHILD: &str = "room";
const ALICE_CHILD: &str = "alice";
const BOB_CHILD: &str = "bob";
const CAROL_CHILD: &str = "carol";

#[derive(Debug, Clone)]
enum ChatCall {
    Join { name: String, actor: ActorId },
}

#[derive(Debug, Clone)]
enum ChatCast {
    Say { from: String, text: String },
    Leave { name: String },
}

#[derive(Debug, Clone)]
enum ChatReply {
    Joined { members: Vec<String> },
    NameTaken(String),
}

#[derive(Debug, Clone)]
enum ChatEvent {
    Joined { name: String },
    Left { name: String },
    Message { from: String, text: String },
}

#[derive(Debug, Default)]
struct RoomState {
    members: BTreeMap<String, ActorId>,
    transcript: Vec<String>,
}

struct ChatRoom;

impl GenServer for ChatRoom {
    type State = RoomState;
    type Call = ChatCall;
    type Cast = ChatCast;
    type Reply = ChatReply;
    type Info = RuntimeInfo;

    fn init<C: Context>(&mut self, _ctx: &mut C) -> Result<Self::State, ExitReason> {
        Ok(RoomState::default())
    }

    fn handle_call<C: Context>(
        &mut self,
        state: &mut Self::State,
        _from: ReplyToken,
        message: Self::Call,
        ctx: &mut C,
    ) -> CallOutcome<Self::Reply> {
        match message {
            ChatCall::Join { name, actor } => {
                if state.members.contains_key(&name) {
                    return CallOutcome::Reply(ChatReply::NameTaken(name));
                }

                for member in state.members.values().copied() {
                    let _ = ctx.send(member, ChatEvent::Joined { name: name.clone() });
                }

                state.members.insert(name.clone(), actor);

                let members = state.members.keys().cloned().collect();
                CallOutcome::Reply(ChatReply::Joined { members })
            }
        }
    }

    fn handle_cast<C: Context>(
        &mut self,
        state: &mut Self::State,
        message: Self::Cast,
        ctx: &mut C,
    ) -> ServerOutcome {
        match message {
            ChatCast::Say { from, text } => {
                state.transcript.push(format!("{from}: {text}"));

                for member in state.members.values().copied() {
                    let _ = ctx.send(
                        member,
                        ChatEvent::Message {
                            from: from.clone(),
                            text: text.clone(),
                        },
                    );
                }
            }
            ChatCast::Leave { name } => {
                if state.members.remove(&name).is_some() {
                    for member in state.members.values().copied() {
                        let _ = ctx.send(member, ChatEvent::Left { name: name.clone() });
                    }
                }
            }
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
}

struct ChatClient {
    name: String,
    room: ActorId,
    script: VecDeque<String>,
    speak_delay: Duration,
    speak_token: TimerToken,
    leave_token: TimerToken,
}

impl ChatClient {
    fn new(name: &str, room: ActorId, script: &[&str], speak_delay: Duration) -> Self {
        Self {
            name: name.to_owned(),
            room,
            script: script.iter().map(|line| (*line).to_owned()).collect(),
            speak_delay,
            speak_token: TimerToken::next(),
            leave_token: TimerToken::next(),
        }
    }

    fn schedule_next_line<C: Context>(&self, ctx: &mut C) -> Result<(), ExitReason> {
        ctx.schedule_after(self.speak_delay, self.speak_token)
            .map_err(|error| ExitReason::Error(format!("schedule send failed: {error:?}")))
    }

    fn schedule_departure<C: Context>(&self, ctx: &mut C) -> Result<(), ExitReason> {
        ctx.schedule_after(self.speak_delay, self.leave_token)
            .map_err(|error| ExitReason::Error(format!("schedule leave failed: {error:?}")))
    }
}

impl Actor for ChatClient {
    fn init<C: Context>(&mut self, ctx: &mut C) -> Result<(), ExitReason> {
        ctx.ask(
            self.room,
            ChatCall::Join {
                name: self.name.clone(),
                actor: ctx.actor_id(),
            },
            None,
        )
        .map_err(|error| ExitReason::Error(format!("join request failed: {error:?}")))?;

        Ok(())
    }

    fn handle<C: Context>(&mut self, envelope: Envelope, ctx: &mut C) -> ActorTurn {
        match envelope {
            Envelope::Reply { message, .. } => {
                let reply = match message.downcast::<ChatReply>() {
                    Ok(reply) => reply,
                    Err(payload) => {
                        return ActorTurn::Stop(ExitReason::Error(format!(
                            "unexpected chat reply `{}`",
                            payload.type_name()
                        )));
                    }
                };

                match reply {
                    ChatReply::Joined { members } => {
                        println!(
                            "{} joined room with members: {}",
                            self.name,
                            members.join(", ")
                        );

                        if let Err(reason) = self.schedule_next_line(ctx) {
                            return ActorTurn::Stop(reason);
                        }
                    }
                    ChatReply::NameTaken(name) => {
                        return ActorTurn::Stop(ExitReason::Error(format!(
                            "chat name `{name}` is already in use"
                        )));
                    }
                }
            }
            Envelope::User(payload) => {
                let event = match payload.downcast::<ChatEvent>() {
                    Ok(event) => event,
                    Err(payload) => {
                        return ActorTurn::Stop(ExitReason::Error(format!(
                            "unexpected chat event `{}`",
                            payload.type_name()
                        )));
                    }
                };

                match event {
                    ChatEvent::Joined { name } => {
                        println!("{} sees {} join", self.name, name);
                    }
                    ChatEvent::Left { name } => {
                        println!("{} sees {} leave", self.name, name);
                    }
                    ChatEvent::Message { from, text } => {
                        println!("{} received <{}> {}", self.name, from, text);
                    }
                }
            }
            Envelope::Timer(timer) if timer.token == self.speak_token => {
                if let Some(line) = self.script.pop_front()
                    && let Err(error) = ctx.send(
                        self.room,
                        CastMessage(ChatCast::Say {
                            from: self.name.clone(),
                            text: line,
                        }),
                    )
                {
                    return ActorTurn::Stop(ExitReason::Error(format!(
                        "send message failed: {error:?}"
                    )));
                }

                let schedule_result = if self.script.is_empty() {
                    self.schedule_departure(ctx)
                } else {
                    self.schedule_next_line(ctx)
                };

                if let Err(reason) = schedule_result {
                    return ActorTurn::Stop(reason);
                }
            }
            Envelope::Timer(timer) if timer.token == self.leave_token => {
                let _ = ctx.send(
                    self.room,
                    CastMessage(ChatCast::Leave {
                        name: self.name.clone(),
                    }),
                );
                println!("{} leaves the room", self.name);
                return ActorTurn::Stop(ExitReason::Normal);
            }
            _ => {}
        }

        ActorTurn::Continue
    }
}

struct ChatDemoSupervisor {
    room: Option<ActorId>,
    specs: Vec<ChildSpec>,
}

impl ChatDemoSupervisor {
    fn new() -> Self {
        Self {
            room: None,
            specs: vec![
                ChildSpec {
                    id: ROOM_CHILD,
                    restart: Restart::Permanent,
                    shutdown: Shutdown::default(),
                    is_supervisor: false,
                },
                ChildSpec {
                    id: ALICE_CHILD,
                    restart: Restart::Temporary,
                    shutdown: Shutdown::default(),
                    is_supervisor: false,
                },
                ChildSpec {
                    id: BOB_CHILD,
                    restart: Restart::Temporary,
                    shutdown: Shutdown::default(),
                    is_supervisor: false,
                },
                ChildSpec {
                    id: CAROL_CHILD,
                    restart: Restart::Temporary,
                    shutdown: Shutdown::default(),
                    is_supervisor: false,
                },
            ],
        }
    }

    fn client_for(&self, child_id: &'static str) -> ChatClient {
        let room = self.room.expect("chat room should start before clients");

        match child_id {
            ALICE_CHILD => ChatClient::new(
                "alice",
                room,
                &["hello everyone", "anyone tried this runtime yet?"],
                Duration::from_millis(20),
            ),
            BOB_CHILD => ChatClient::new(
                "bob",
                room,
                &["hey alice", "the OTP shape is already pretty nice"],
                Duration::from_millis(35),
            ),
            CAROL_CHILD => ChatClient::new(
                "carol",
                room,
                &["I joined late, but this looks good"],
                Duration::from_millis(50),
            ),
            other => panic!("unknown chat child `{other}`"),
        }
    }
}

impl Supervisor for ChatDemoSupervisor {
    fn flags(&self) -> SupervisorFlags {
        SupervisorFlags {
            strategy: Strategy::OneForOne,
            ..SupervisorFlags::default()
        }
    }

    fn child_specs(&self) -> &[ChildSpec] {
        &self.specs
    }

    fn start_child<C: Context>(
        &mut self,
        spec: &ChildSpec,
        ctx: &mut C,
    ) -> Result<ActorId, StartChildError> {
        let actor = match spec.id {
            ROOM_CHILD => ctx
                .spawn_gen_server(ChatRoom, Default::default())
                .map_err(|_| StartChildError::SpawnRejected)?,
            ALICE_CHILD | BOB_CHILD | CAROL_CHILD => ctx
                .spawn(self.client_for(spec.id), Default::default())
                .map_err(|_| StartChildError::SpawnRejected)?,
            other => {
                return Err(StartChildError::InitFailed(ExitReason::Error(format!(
                    "unknown chat child `{other}`"
                ))));
            }
        };

        if spec.id == ROOM_CHILD {
            self.room = Some(actor);
        }

        Ok(actor)
    }

    fn on_child_exit<C: Context>(
        &mut self,
        spec: &ChildSpec,
        _actor: ActorId,
        reason: ExitReason,
        _ctx: &mut C,
    ) -> SupervisorDirective {
        if spec.id == ROOM_CHILD {
            self.room = None;
        }

        if !spec.should_restart(&reason) {
            return SupervisorDirective::Ignore;
        }

        if spec.id == ROOM_CHILD {
            return SupervisorDirective::Restart(self.specs.iter().map(|child| child.id).collect());
        }

        SupervisorDirective::Restart(vec![spec.id])
    }
}

fn main() {
    let mut runtime = LocalRuntime::default();
    let root = runtime
        .spawn_supervisor(ChatDemoSupervisor::new())
        .expect("chat demo supervisor should spawn");

    runtime.run_until_idle();

    while runtime.block_on_next(Some(Duration::from_millis(250))) {}

    let _ = runtime.exit_actor(root, ExitReason::Shutdown);
    runtime.run_until_idle();

    println!("chat example complete");
}
