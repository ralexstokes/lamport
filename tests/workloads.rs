use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

mod support;

use lamport::{
    Actor, ActorId, ActorTurn, Application, CallOutcome, CastMessage, ChildSpec, ConcurrentRuntime,
    Context, Envelope, ExitReason, GenServer, LocalRuntime, ReplyToken, Restart, RuntimeInfo,
    ServerOutcome, SpawnOptions, StartChildError, Strategy, Supervisor, SupervisorDirective,
    SupervisorFlags, TimerToken, boot_concurrent_application, restart_scope,
};
use support::map_spawn_error;

const CHAT_LEAVE_DELAY: Duration = Duration::from_millis(40);
const CHAT_SPEAK_DELAY: Duration = Duration::from_millis(5);
const SERVICE_CHILD: &str = "counter";
const SERVICE_NAME: &str = "service.counter";
const SERVICE_ROOT: &str = "service.root";

#[derive(Debug, Clone, PartialEq, Eq)]
enum ChatCall {
    Join { name: String, actor: ActorId },
    Snapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ChatCast {
    Say { from: String, text: String },
    Leave { name: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ChatReply {
    Joined,
    Snapshot {
        members: Vec<String>,
        transcript: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ChatEvent {
    Joined { name: String },
    Left { name: String },
    Message { from: String, text: String },
}

#[derive(Debug, Default)]
struct ChatState {
    members: BTreeMap<String, ActorId>,
    transcript: Vec<String>,
}

struct ChatRoom;

impl GenServer for ChatRoom {
    type State = ChatState;
    type Call = ChatCall;
    type Cast = ChatCast;
    type Reply = ChatReply;
    type Info = RuntimeInfo;

    fn init<C: Context>(&mut self, _ctx: &mut C) -> Result<Self::State, ExitReason> {
        Ok(ChatState::default())
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
                for member in state.members.values().copied() {
                    let _ = ctx.send(member, ChatEvent::Joined { name: name.clone() });
                }

                state.members.insert(name, actor);
                CallOutcome::Reply(ChatReply::Joined)
            }
            ChatCall::Snapshot => CallOutcome::Reply(ChatReply::Snapshot {
                members: state.members.keys().cloned().collect(),
                transcript: state.transcript.clone(),
            }),
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
                state.transcript.push(format!("{from}:{text}"));

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
    speak_token: TimerToken,
    leave_token: TimerToken,
    log: Arc<Mutex<Vec<String>>>,
}

impl ChatClient {
    fn new(name: &str, room: ActorId, script: &[&str], log: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            name: name.to_owned(),
            room,
            script: script.iter().map(|line| (*line).to_owned()).collect(),
            speak_token: TimerToken::next(),
            leave_token: TimerToken::next(),
            log,
        }
    }

    fn schedule_speak<C: Context>(&self, ctx: &mut C) -> Result<(), ExitReason> {
        ctx.schedule_after(CHAT_SPEAK_DELAY, self.speak_token)
            .map_err(|error| ExitReason::Error(format!("schedule speak failed: {error:?}")))
    }

    fn schedule_leave<C: Context>(&self, ctx: &mut C) -> Result<(), ExitReason> {
        ctx.schedule_after(CHAT_LEAVE_DELAY, self.leave_token)
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
        .map_err(|error| ExitReason::Error(format!("join failed: {error:?}")))?;
        Ok(())
    }

    fn handle<C: Context>(&mut self, envelope: Envelope, ctx: &mut C) -> ActorTurn {
        match envelope {
            Envelope::Reply { message, .. } => {
                let reply = message.downcast::<ChatReply>().ok().unwrap();
                if let ChatReply::Joined = reply {
                    if let Err(reason) = self.schedule_speak(ctx) {
                        return ActorTurn::Stop(reason);
                    }
                }
            }
            Envelope::User(payload) => {
                let event = payload.downcast::<ChatEvent>().ok().unwrap();
                let line = match event {
                    ChatEvent::Joined { name } => format!("join:{name}"),
                    ChatEvent::Left { name } => format!("left:{name}"),
                    ChatEvent::Message { from, text } => format!("msg:{from}:{text}"),
                };
                self.log.lock().unwrap().push(line);
            }
            Envelope::Timer(timer) if timer.token == self.speak_token => {
                if let Some(line) = self.script.pop_front() {
                    if let Err(error) = ctx.send(
                        self.room,
                        CastMessage(ChatCast::Say {
                            from: self.name.clone(),
                            text: line,
                        }),
                    ) {
                        return ActorTurn::Stop(ExitReason::Error(format!(
                            "chat send failed: {error:?}"
                        )));
                    }
                }

                let scheduled = if self.script.is_empty() {
                    self.schedule_leave(ctx)
                } else {
                    self.schedule_speak(ctx)
                };
                if let Err(reason) = scheduled {
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
                return ActorTurn::Stop(ExitReason::Normal);
            }
            _ => {}
        }

        ActorTurn::Continue
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CapturedChatSnapshot {
    members: Vec<String>,
    transcript: Vec<String>,
}

struct ChatSnapshotClient {
    room: ActorId,
    seen: Arc<Mutex<Option<CapturedChatSnapshot>>>,
}

impl Actor for ChatSnapshotClient {
    fn init<C: Context>(&mut self, ctx: &mut C) -> Result<(), ExitReason> {
        ctx.ask(self.room, ChatCall::Snapshot, None)
            .map_err(|error| ExitReason::Error(format!("snapshot failed: {error:?}")))?;
        Ok(())
    }

    fn handle<C: Context>(&mut self, envelope: Envelope, _ctx: &mut C) -> ActorTurn {
        if let Envelope::Reply { message, .. } = envelope {
            let snapshot = message.downcast::<ChatReply>().ok().unwrap();
            if let ChatReply::Snapshot {
                members,
                transcript,
            } = snapshot
            {
                *self.seen.lock().unwrap() = Some(CapturedChatSnapshot {
                    members,
                    transcript,
                });
                return ActorTurn::Stop(ExitReason::Normal);
            }
        }

        ActorTurn::Continue
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CounterCall {
    Add(u64),
    Get,
}

struct CounterService;

impl GenServer for CounterService {
    type State = u64;
    type Call = CounterCall;
    type Cast = ();
    type Reply = u64;
    type Info = RuntimeInfo;

    fn init<C: Context>(&mut self, _ctx: &mut C) -> Result<Self::State, ExitReason> {
        Ok(0)
    }

    fn handle_call<C: Context>(
        &mut self,
        state: &mut Self::State,
        _from: ReplyToken,
        message: Self::Call,
        _ctx: &mut C,
    ) -> CallOutcome<Self::Reply> {
        match message {
            CounterCall::Add(value) => {
                *state += value;
                CallOutcome::Reply(*state)
            }
            CounterCall::Get => CallOutcome::Reply(*state),
        }
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
        _state: &mut Self::State,
        _message: Self::Info,
        _ctx: &mut C,
    ) -> ServerOutcome {
        ServerOutcome::Continue
    }
}

struct ServiceClient {
    results: Arc<Mutex<Vec<u64>>>,
}

impl Actor for ServiceClient {
    fn init<C: Context>(&mut self, ctx: &mut C) -> Result<(), ExitReason> {
        let service = ctx
            .whereis(SERVICE_NAME)
            .ok_or_else(|| ExitReason::Error("service not registered".into()))?;
        ctx.ask(service, CounterCall::Add(1), None)
            .map_err(|error| ExitReason::Error(format!("service call failed: {error:?}")))?;
        Ok(())
    }

    fn handle<C: Context>(&mut self, envelope: Envelope, _ctx: &mut C) -> ActorTurn {
        if let Envelope::Reply { message, .. } = envelope {
            self.results
                .lock()
                .unwrap()
                .push(message.downcast::<u64>().ok().unwrap());
            return ActorTurn::Stop(ExitReason::Normal);
        }

        ActorTurn::Continue
    }
}

struct ServiceSnapshotClient {
    seen: Arc<Mutex<Vec<u64>>>,
}

impl Actor for ServiceSnapshotClient {
    fn init<C: Context>(&mut self, ctx: &mut C) -> Result<(), ExitReason> {
        let service = ctx
            .whereis(SERVICE_NAME)
            .ok_or_else(|| ExitReason::Error("service not registered".into()))?;
        ctx.ask(service, CounterCall::Get, None)
            .map_err(|error| ExitReason::Error(format!("snapshot failed: {error:?}")))?;
        Ok(())
    }

    fn handle<C: Context>(&mut self, envelope: Envelope, _ctx: &mut C) -> ActorTurn {
        if let Envelope::Reply { message, .. } = envelope {
            self.seen
                .lock()
                .unwrap()
                .push(message.downcast::<u64>().ok().unwrap());
            return ActorTurn::Stop(ExitReason::Normal);
        }

        ActorTurn::Continue
    }
}

struct ServiceSupervisor {
    flags: SupervisorFlags,
    specs: Vec<ChildSpec>,
}

impl ServiceSupervisor {
    fn new() -> Self {
        Self {
            flags: SupervisorFlags {
                strategy: Strategy::OneForOne,
                ..SupervisorFlags::default()
            },
            specs: vec![ChildSpec {
                id: SERVICE_CHILD,
                restart: Restart::Permanent,
                shutdown: Default::default(),
                is_supervisor: false,
            }],
        }
    }
}

impl Supervisor for ServiceSupervisor {
    fn flags(&self) -> SupervisorFlags {
        self.flags.clone()
    }

    fn child_specs(&self) -> &[ChildSpec] {
        &self.specs
    }

    fn start_child<C: Context>(
        &mut self,
        _spec: &ChildSpec,
        ctx: &mut C,
    ) -> Result<ActorId, StartChildError> {
        ctx.spawn_gen_server(
            CounterService,
            SpawnOptions {
                registered_name: Some(SERVICE_NAME.into()),
                ..SpawnOptions::default()
            },
        )
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

struct ServiceApplication;

impl Application for ServiceApplication {
    type RootSupervisor = ServiceSupervisor;

    fn name(&self) -> &'static str {
        "service-app"
    }

    fn root_options(&self) -> SpawnOptions {
        SpawnOptions {
            registered_name: Some(SERVICE_ROOT.into()),
            ..SpawnOptions::default()
        }
    }

    fn root_supervisor(self) -> Self::RootSupervisor {
        ServiceSupervisor::new()
    }
}

fn wait_for_local_actors(runtime: &mut LocalRuntime, actors: &[ActorId]) {
    let deadline = Instant::now() + Duration::from_secs(2);

    while Instant::now() < deadline {
        if actors.iter().all(|actor| {
            runtime
                .actor_snapshot(*actor)
                .is_some_and(|snapshot| snapshot.status == lamport::ActorStatus::Dead)
        }) {
            return;
        }

        let _ = runtime.block_on_next(Some(Duration::from_millis(50)));
        runtime.run_until_idle();
    }

    panic!("actors did not finish in time");
}

fn wait_for_results(runtime: &ConcurrentRuntime, results: &Arc<Mutex<Vec<u64>>>, expected: usize) {
    let deadline = Instant::now() + Duration::from_secs(3);

    while Instant::now() < deadline {
        if results.lock().unwrap().len() == expected {
            return;
        }

        let _ = runtime.wait_for_idle(Some(Duration::from_millis(50)));
        thread::sleep(Duration::from_millis(10));
    }

    panic!("concurrent workload did not complete in time");
}

fn sorted(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values
}

#[test]
fn chat_style_workload_preserves_membership_and_message_flow() {
    let mut runtime = LocalRuntime::default();
    let room = runtime.spawn_gen_server(ChatRoom).unwrap();

    let alice_log = Arc::new(Mutex::new(Vec::new()));
    let bob_log = Arc::new(Mutex::new(Vec::new()));
    let carol_log = Arc::new(Mutex::new(Vec::new()));

    let actors = vec![
        runtime
            .spawn(ChatClient::new(
                "alice",
                room,
                &["hello", "bye"],
                Arc::clone(&alice_log),
            ))
            .unwrap(),
        runtime
            .spawn(ChatClient::new(
                "bob",
                room,
                &["one", "two"],
                Arc::clone(&bob_log),
            ))
            .unwrap(),
        runtime
            .spawn(ChatClient::new(
                "carol",
                room,
                &["red", "blue"],
                Arc::clone(&carol_log),
            ))
            .unwrap(),
    ];

    runtime.run_until_idle();
    wait_for_local_actors(&mut runtime, &actors);

    let snapshot = Arc::new(Mutex::new(None));
    runtime
        .spawn(ChatSnapshotClient {
            room,
            seen: Arc::clone(&snapshot),
        })
        .unwrap();
    runtime.run_until_idle();

    let snapshot = snapshot.lock().unwrap();
    let snapshot = snapshot.as_ref().unwrap();
    assert!(snapshot.members.is_empty());
    let expected = vec![
        "alice:hello".to_owned(),
        "alice:bye".to_owned(),
        "bob:one".to_owned(),
        "bob:two".to_owned(),
        "carol:red".to_owned(),
        "carol:blue".to_owned(),
    ];
    assert_eq!(sorted(snapshot.transcript.clone()), sorted(expected));

    let alice = alice_log.lock().unwrap().clone();
    let bob = bob_log.lock().unwrap().clone();
    let carol = carol_log.lock().unwrap().clone();

    assert!(alice.iter().any(|event| event == "join:bob"));
    assert!(alice.iter().any(|event| event == "join:carol"));
    assert!(alice.iter().any(|event| event == "msg:bob:one"));
    assert!(alice.iter().any(|event| event == "msg:carol:red"));
    assert!(bob.iter().any(|event| event == "msg:alice:hello"));
    assert!(bob.iter().any(|event| event == "msg:carol:red"));
    assert!(carol.iter().any(|event| event == "msg:alice:hello"));
    assert!(carol.iter().any(|event| event == "msg:bob:one"));
}

#[test]
fn service_style_workload_handles_parallel_request_reply_traffic() {
    let (runtime, handle) =
        boot_concurrent_application(ServiceApplication, Default::default()).unwrap();
    assert_eq!(handle.name(), "service-app");
    assert!(runtime.wait_for_idle(Some(Duration::from_secs(2))));
    assert_eq!(
        runtime.resolve_name(SERVICE_ROOT),
        Some(handle.root_supervisor())
    );
    assert!(runtime.resolve_name(SERVICE_NAME).is_some());

    let replies = Arc::new(Mutex::new(Vec::new()));
    for _ in 0..32 {
        runtime
            .spawn(ServiceClient {
                results: Arc::clone(&replies),
            })
            .unwrap();
    }

    wait_for_results(&runtime, &replies, 32);

    let mut replies = replies.lock().unwrap().clone();
    replies.sort_unstable();
    assert_eq!(replies, (1_u64..=32).collect::<Vec<_>>());

    let snapshot = Arc::new(Mutex::new(Vec::new()));
    runtime
        .spawn(ServiceSnapshotClient {
            seen: Arc::clone(&snapshot),
        })
        .unwrap();
    wait_for_results(&runtime, &snapshot, 1);
    assert_eq!(snapshot.lock().unwrap().as_slice(), &[32]);
}
