use crate::{
    actor::{Actor, ActorTurn},
    context::Context,
    envelope::{
        CallTimedOut, DownMessage, Envelope, ExitSignal, Message, Payload, ReplyToken,
        SystemMessage, TaskCompleted, TimerFired,
    },
    types::{ExitReason, Ref},
};

/// Outcome of handling a synchronous `call`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallOutcome<R> {
    /// Reply and continue running.
    Reply(R),
    /// Reply and then stop with an exit reason.
    ReplyAndStop {
        /// The reply sent to the caller.
        reply: R,
        /// The reason the server should terminate.
        reason: ExitReason,
    },
    /// Delay the reply and continue running.
    NoReply,
    /// Stop without sending a reply.
    Stop(ExitReason),
}

/// Outcome of handling an async cast or info message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerOutcome {
    /// Continue running.
    Continue,
    /// Yield after the current turn.
    Yield,
    /// Stop the server.
    Stop(ExitReason),
}

/// Outcome of handling a synchronous `GenStatem` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatemCallOutcome<S, R> {
    /// Reply and keep the current state.
    Reply(R),
    /// Reply and transition to a new state.
    ReplyAndTransition {
        /// Reply sent to the caller.
        reply: R,
        /// Next state for the machine.
        state: S,
    },
    /// Delay the reply and keep the current state.
    NoReply,
    /// Delay the reply and transition to a new state.
    NoReplyAndTransition(S),
    /// Reply and then stop with the given reason.
    ReplyAndStop {
        /// Reply sent to the caller.
        reply: R,
        /// Final exit reason for the machine.
        reason: ExitReason,
    },
    /// Stop without sending a reply.
    Stop(ExitReason),
}

/// Outcome of handling a `GenStatem` cast or info message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatemOutcome<S> {
    /// Continue in the current state.
    Continue,
    /// Transition to a new state and continue.
    Transition(S),
    /// Yield after the current turn.
    Yield,
    /// Transition to a new state and yield.
    TransitionAndYield(S),
    /// Stop the machine.
    Stop(ExitReason),
}

/// Wrapper used for `cast` messages sent to a `GenServerActor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CastMessage<T>(pub T);

/// Wrapper used for user-defined `info` messages sent to a `GenServerActor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfoMessage<T>(pub T);

/// Runtime-generated informational messages delivered to `GenServer::handle_info`.
#[derive(Debug)]
pub enum RuntimeInfo {
    /// Reply to a previously issued call.
    Reply {
        /// The original request reference.
        reference: Ref,
        /// The reply payload.
        message: Payload,
    },
    /// Completion of a blocking task.
    Task(TaskCompleted),
    /// Timeout for a pending request.
    CallTimeout(CallTimedOut),
    /// Exit signal from a linked actor.
    Exit(ExitSignal),
    /// Down notification from a monitor.
    Down(DownMessage),
    /// Fired timer token.
    Timer(TimerFired),
    /// Reserved runtime control message.
    System(SystemMessage),
}

/// OTP-style typed server behaviour layered on top of actors.
pub trait GenServer: Send + 'static {
    /// Mutable server state owned by the behaviour.
    type State;
    /// Synchronous request message type.
    type Call: Message;
    /// Async cast message type.
    type Cast: Message;
    /// Reply message type.
    type Reply: Message;
    /// Info message type, typically a user-defined enum over system events.
    type Info: Message;

    /// Initializes the server and returns the initial state.
    fn init<C: Context>(&mut self, ctx: &mut C) -> Result<Self::State, ExitReason>;

    /// Handles a synchronous request.
    fn handle_call<C: Context>(
        &mut self,
        state: &mut Self::State,
        from: ReplyToken,
        message: Self::Call,
        ctx: &mut C,
    ) -> CallOutcome<Self::Reply>;

    /// Handles an async cast.
    fn handle_cast<C: Context>(
        &mut self,
        state: &mut Self::State,
        message: Self::Cast,
        ctx: &mut C,
    ) -> ServerOutcome;

    /// Handles an informational message such as a timer, exit, or down notification.
    fn handle_info<C: Context>(
        &mut self,
        state: &mut Self::State,
        message: Self::Info,
        ctx: &mut C,
    ) -> ServerOutcome;

    /// Runs once when the server terminates.
    fn terminate<C: Context>(
        &mut self,
        _state: &mut Self::State,
        _reason: ExitReason,
        _ctx: &mut C,
    ) {
    }

    /// Runs during a future code-change flow.
    fn code_change<C: Context>(
        &mut self,
        state: Self::State,
        _ctx: &mut C,
    ) -> Result<Self::State, ExitReason> {
        Ok(state)
    }
}

/// OTP-style typed state-machine behaviour layered on top of actors.
pub trait GenStatem: Send + 'static {
    /// Current state identifier for the machine.
    type State: Send + 'static;
    /// Mutable data carried across state transitions.
    type Data: Send + 'static;
    /// Synchronous request message type.
    type Call: Message;
    /// Async cast message type.
    type Cast: Message;
    /// Reply message type.
    type Reply: Message;
    /// Info message type, typically a user-defined enum over system events.
    type Info: Message;

    /// Initializes the machine and returns its starting state and data.
    fn init<C: Context>(&mut self, ctx: &mut C) -> Result<(Self::State, Self::Data), ExitReason>;

    /// Handles a synchronous request in the current state.
    fn handle_call<C: Context>(
        &mut self,
        state: &mut Self::State,
        data: &mut Self::Data,
        from: ReplyToken,
        message: Self::Call,
        ctx: &mut C,
    ) -> StatemCallOutcome<Self::State, Self::Reply>;

    /// Handles an async cast in the current state.
    fn handle_cast<C: Context>(
        &mut self,
        state: &mut Self::State,
        data: &mut Self::Data,
        message: Self::Cast,
        ctx: &mut C,
    ) -> StatemOutcome<Self::State>;

    /// Handles an info message such as a timer, exit, or down notification.
    fn handle_info<C: Context>(
        &mut self,
        state: &mut Self::State,
        data: &mut Self::Data,
        message: Self::Info,
        ctx: &mut C,
    ) -> StatemOutcome<Self::State>;

    /// Runs once when the machine terminates.
    fn terminate<C: Context>(
        &mut self,
        _state: &mut Self::State,
        _data: &mut Self::Data,
        _reason: ExitReason,
        _ctx: &mut C,
    ) {
    }

    /// Runs during a future code-change flow.
    fn code_change<C: Context>(
        &mut self,
        state: Self::State,
        data: Self::Data,
        _ctx: &mut C,
    ) -> Result<(Self::State, Self::Data), ExitReason> {
        Ok((state, data))
    }
}

/// Actor adapter that makes a typed `GenServer` spawnable on the raw runtime.
pub struct GenServerActor<G: GenServer>
where
    G::Info: From<RuntimeInfo>,
    G::State: Send,
{
    server: G,
    state: Option<G::State>,
}

/// Actor adapter that makes a typed `GenStatem` spawnable on the raw runtime.
pub struct GenStatemActor<G: GenStatem>
where
    G::Info: From<RuntimeInfo>,
{
    machine: G,
    state: Option<G::State>,
    data: Option<G::Data>,
}

impl<G> GenServerActor<G>
where
    G: GenServer,
    G::Info: From<RuntimeInfo>,
    G::State: Send,
{
    /// Wraps a typed server as a low-level actor.
    pub fn new(server: G) -> Self {
        Self {
            server,
            state: None,
        }
    }
}

impl<G> GenStatemActor<G>
where
    G: GenStatem,
    G::Info: From<RuntimeInfo>,
{
    /// Wraps a typed state machine as a low-level actor.
    pub fn new(machine: G) -> Self {
        Self {
            machine,
            state: None,
            data: None,
        }
    }
}

impl<G> Actor for GenServerActor<G>
where
    G: GenServer,
    G::Info: From<RuntimeInfo>,
    G::State: Send,
{
    fn name(&self) -> &'static str {
        std::any::type_name::<G>()
    }

    fn init<C: Context>(&mut self, ctx: &mut C) -> Result<(), ExitReason> {
        self.state = Some(self.server.init(ctx)?);
        Ok(())
    }

    fn handle<C: Context>(&mut self, envelope: Envelope, ctx: &mut C) -> ActorTurn {
        match envelope {
            Envelope::Request { token, message } => self.handle_call(token, message, ctx),
            Envelope::User(payload) => self.handle_user(payload, ctx),
            Envelope::Reply { reference, message } => {
                self.handle_runtime_info(RuntimeInfo::Reply { reference, message }, ctx)
            }
            Envelope::Task(task) => self.handle_runtime_info(RuntimeInfo::Task(task), ctx),
            Envelope::CallTimeout(timeout) => {
                self.handle_runtime_info(RuntimeInfo::CallTimeout(timeout), ctx)
            }
            Envelope::Exit(signal) => self.handle_runtime_info(RuntimeInfo::Exit(signal), ctx),
            Envelope::Down(message) => self.handle_runtime_info(RuntimeInfo::Down(message), ctx),
            Envelope::Timer(timer) => self.handle_runtime_info(RuntimeInfo::Timer(timer), ctx),
            Envelope::System(message) => {
                self.handle_runtime_info(RuntimeInfo::System(message), ctx)
            }
        }
    }

    fn terminate<C: Context>(&mut self, reason: ExitReason, ctx: &mut C) {
        if let Some(state) = self.state.as_mut() {
            self.server.terminate(state, reason, ctx);
        }
    }
}

impl<G> Actor for GenStatemActor<G>
where
    G: GenStatem,
    G::Info: From<RuntimeInfo>,
{
    fn name(&self) -> &'static str {
        std::any::type_name::<G>()
    }

    fn init<C: Context>(&mut self, ctx: &mut C) -> Result<(), ExitReason> {
        let (state, data) = self.machine.init(ctx)?;
        self.state = Some(state);
        self.data = Some(data);
        Ok(())
    }

    fn handle<C: Context>(&mut self, envelope: Envelope, ctx: &mut C) -> ActorTurn {
        match envelope {
            Envelope::Request { token, message } => self.handle_call(token, message, ctx),
            Envelope::User(payload) => self.handle_user(payload, ctx),
            Envelope::Reply { reference, message } => {
                self.handle_runtime_info(RuntimeInfo::Reply { reference, message }, ctx)
            }
            Envelope::Task(task) => self.handle_runtime_info(RuntimeInfo::Task(task), ctx),
            Envelope::CallTimeout(timeout) => {
                self.handle_runtime_info(RuntimeInfo::CallTimeout(timeout), ctx)
            }
            Envelope::Exit(signal) => self.handle_runtime_info(RuntimeInfo::Exit(signal), ctx),
            Envelope::Down(message) => self.handle_runtime_info(RuntimeInfo::Down(message), ctx),
            Envelope::Timer(timer) => self.handle_runtime_info(RuntimeInfo::Timer(timer), ctx),
            Envelope::System(message) => {
                self.handle_runtime_info(RuntimeInfo::System(message), ctx)
            }
        }
    }

    fn terminate<C: Context>(&mut self, reason: ExitReason, ctx: &mut C) {
        if let (Some(state), Some(data)) = (self.state.as_mut(), self.data.as_mut()) {
            self.machine.terminate(state, data, reason, ctx);
        }
    }
}

impl<G> GenServerActor<G>
where
    G: GenServer,
    G::Info: From<RuntimeInfo>,
    G::State: Send,
{
    fn server_and_state_mut(&mut self) -> (&mut G, &mut G::State) {
        let Self { server, state } = self;
        let state = state
            .as_mut()
            .expect("gen server state must be initialized before handling messages");
        (server, state)
    }

    fn handle_call<C: Context>(
        &mut self,
        token: ReplyToken,
        message: Payload,
        ctx: &mut C,
    ) -> ActorTurn {
        let call = match message.downcast::<G::Call>() {
            Ok(call) => call,
            Err(message) => {
                return ActorTurn::Stop(ExitReason::Error(format!(
                    "unexpected call payload `{}`",
                    message.type_name()
                )));
            }
        };

        let (server, state) = self.server_and_state_mut();
        match server.handle_call(state, token, call, ctx) {
            CallOutcome::Reply(reply) => {
                let _ = ctx.reply(token, reply);
                ActorTurn::Continue
            }
            CallOutcome::ReplyAndStop { reply, reason } => {
                let _ = ctx.reply(token, reply);
                ActorTurn::Stop(reason)
            }
            CallOutcome::NoReply => ActorTurn::Continue,
            CallOutcome::Stop(reason) => ActorTurn::Stop(reason),
        }
    }

    fn handle_user<C: Context>(&mut self, payload: Payload, ctx: &mut C) -> ActorTurn {
        match payload.downcast::<CastMessage<G::Cast>>() {
            Ok(CastMessage(message)) => {
                let (server, state) = self.server_and_state_mut();
                map_server_outcome(server.handle_cast(state, message, ctx))
            }
            Err(payload) => match payload.downcast::<InfoMessage<G::Info>>() {
                Ok(InfoMessage(message)) => {
                    let (server, state) = self.server_and_state_mut();
                    map_server_outcome(server.handle_info(state, message, ctx))
                }
                Err(payload) => ActorTurn::Stop(ExitReason::Error(format!(
                    "unexpected user payload `{}`",
                    payload.type_name()
                ))),
            },
        }
    }

    fn handle_runtime_info<C: Context>(&mut self, info: RuntimeInfo, ctx: &mut C) -> ActorTurn {
        let (server, state) = self.server_and_state_mut();
        map_server_outcome(server.handle_info(state, G::Info::from(info), ctx))
    }
}

impl<G> GenStatemActor<G>
where
    G: GenStatem,
    G::Info: From<RuntimeInfo>,
{
    fn machine_state_and_data_mut(&mut self) -> (&mut G, &mut G::State, &mut G::Data) {
        let Self {
            machine,
            state,
            data,
        } = self;
        let state = state
            .as_mut()
            .expect("gen statem state must be initialized before handling messages");
        let data = data
            .as_mut()
            .expect("gen statem data must be initialized before handling messages");
        (machine, state, data)
    }

    fn handle_call<C: Context>(
        &mut self,
        token: ReplyToken,
        message: Payload,
        ctx: &mut C,
    ) -> ActorTurn {
        let call = match message.downcast::<G::Call>() {
            Ok(call) => call,
            Err(message) => {
                return ActorTurn::Stop(ExitReason::Error(format!(
                    "unexpected statem call payload `{}`",
                    message.type_name()
                )));
            }
        };

        let (machine, state, data) = self.machine_state_and_data_mut();
        match machine.handle_call(state, data, token, call, ctx) {
            StatemCallOutcome::Reply(reply) => {
                let _ = ctx.reply(token, reply);
                ActorTurn::Continue
            }
            StatemCallOutcome::ReplyAndTransition { reply, state: next } => {
                *state = next;
                let _ = ctx.reply(token, reply);
                ActorTurn::Continue
            }
            StatemCallOutcome::NoReply => ActorTurn::Continue,
            StatemCallOutcome::NoReplyAndTransition(next) => {
                *state = next;
                ActorTurn::Continue
            }
            StatemCallOutcome::ReplyAndStop { reply, reason } => {
                let _ = ctx.reply(token, reply);
                ActorTurn::Stop(reason)
            }
            StatemCallOutcome::Stop(reason) => ActorTurn::Stop(reason),
        }
    }

    fn handle_user<C: Context>(&mut self, payload: Payload, ctx: &mut C) -> ActorTurn {
        match payload.downcast::<CastMessage<G::Cast>>() {
            Ok(CastMessage(message)) => {
                let (machine, state, data) = self.machine_state_and_data_mut();
                let outcome = machine.handle_cast(state, data, message, ctx);
                map_statem_outcome(state, outcome)
            }
            Err(payload) => match payload.downcast::<InfoMessage<G::Info>>() {
                Ok(InfoMessage(message)) => {
                    let (machine, state, data) = self.machine_state_and_data_mut();
                    let outcome = machine.handle_info(state, data, message, ctx);
                    map_statem_outcome(state, outcome)
                }
                Err(payload) => ActorTurn::Stop(ExitReason::Error(format!(
                    "unexpected statem user payload `{}`",
                    payload.type_name()
                ))),
            },
        }
    }

    fn handle_runtime_info<C: Context>(&mut self, info: RuntimeInfo, ctx: &mut C) -> ActorTurn {
        let (machine, state, data) = self.machine_state_and_data_mut();
        let outcome = machine.handle_info(state, data, G::Info::from(info), ctx);
        map_statem_outcome(state, outcome)
    }
}

fn map_server_outcome(outcome: ServerOutcome) -> ActorTurn {
    match outcome {
        ServerOutcome::Continue => ActorTurn::Continue,
        ServerOutcome::Yield => ActorTurn::Yield,
        ServerOutcome::Stop(reason) => ActorTurn::Stop(reason),
    }
}

fn map_statem_outcome<S>(state: &mut S, outcome: StatemOutcome<S>) -> ActorTurn {
    match outcome {
        StatemOutcome::Continue => ActorTurn::Continue,
        StatemOutcome::Transition(next) => {
            *state = next;
            ActorTurn::Continue
        }
        StatemOutcome::Yield => ActorTurn::Yield,
        StatemOutcome::TransitionAndYield(next) => {
            *state = next;
            ActorTurn::Yield
        }
        StatemOutcome::Stop(reason) => ActorTurn::Stop(reason),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crate::{
        ActorTurn, Context, Envelope, ExitReason, LocalRuntime, ReplyToken, context::PendingCall,
    };

    use super::{
        CallOutcome, CastMessage, GenServer, GenServerActor, GenStatem, GenStatemActor,
        InfoMessage, RuntimeInfo, ServerOutcome, StatemCallOutcome, StatemOutcome,
    };

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum CounterCall {
        Add(i32),
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum CounterCast {
        Add(i32),
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum CounterInfo {
        User(&'static str),
        Runtime(&'static str),
    }

    impl From<RuntimeInfo> for CounterInfo {
        fn from(info: RuntimeInfo) -> Self {
            match info {
                RuntimeInfo::Reply { .. } => Self::Runtime("reply"),
                RuntimeInfo::Task(_) => Self::Runtime("task"),
                RuntimeInfo::CallTimeout(_) => Self::Runtime("timeout"),
                RuntimeInfo::Exit(_) => Self::Runtime("exit"),
                RuntimeInfo::Down(_) => Self::Runtime("down"),
                RuntimeInfo::Timer(_) => Self::Runtime("timer"),
                RuntimeInfo::System(_) => Self::Runtime("system"),
            }
        }
    }

    struct CounterServer;

    impl GenServer for CounterServer {
        type State = i32;
        type Call = CounterCall;
        type Cast = CounterCast;
        type Reply = i32;
        type Info = CounterInfo;

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
            }
        }

        fn handle_cast<C: Context>(
            &mut self,
            state: &mut Self::State,
            message: Self::Cast,
            _ctx: &mut C,
        ) -> ServerOutcome {
            match message {
                CounterCast::Add(value) => {
                    *state += value;
                    ServerOutcome::Continue
                }
            }
        }

        fn handle_info<C: Context>(
            &mut self,
            _state: &mut Self::State,
            message: Self::Info,
            _ctx: &mut C,
        ) -> ServerOutcome {
            match message {
                CounterInfo::User(_) | CounterInfo::Runtime(_) => ServerOutcome::Continue,
            }
        }
    }

    struct ClientActor {
        server: crate::ActorId,
        seen: Arc<Mutex<Vec<i32>>>,
        pending: Option<PendingCall>,
    }

    impl crate::Actor for ClientActor {
        fn init<C: Context>(&mut self, ctx: &mut C) -> Result<(), ExitReason> {
            self.pending = Some(
                ctx.ask(self.server, CounterCall::Add(2), None)
                    .map_err(|error| ExitReason::Error(format!("ask failed: {error:?}")))?,
            );
            Ok(())
        }

        fn handle<C: Context>(&mut self, envelope: Envelope, _ctx: &mut C) -> ActorTurn {
            if let Envelope::Reply { message, .. } = envelope {
                self.seen
                    .lock()
                    .unwrap()
                    .push(message.downcast::<i32>().ok().unwrap());
            }

            ActorTurn::Continue
        }
    }

    #[test]
    fn gen_server_actor_handles_casts_and_calls() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = LocalRuntime::default();
        let server = runtime.spawn(GenServerActor::new(CounterServer)).unwrap();

        runtime.run_until_idle();
        runtime
            .send(server, CastMessage(CounterCast::Add(3)))
            .unwrap();
        runtime
            .spawn(ClientActor {
                server,
                seen: Arc::clone(&seen),
                pending: None,
            })
            .unwrap();
        runtime
            .send(server, InfoMessage(CounterInfo::User("noop")))
            .unwrap();
        runtime.run_until_idle();

        assert_eq!(seen.lock().unwrap().as_slice(), &[5]);
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum GateState {
        Closed,
        Open,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum GateCall {
        Snapshot,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum GateCast {
        Unlock,
        Push,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct GateSnapshot {
        state: GateState,
        pushes: usize,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum GateInfo {
        User(&'static str),
        Runtime(&'static str),
    }

    impl From<RuntimeInfo> for GateInfo {
        fn from(info: RuntimeInfo) -> Self {
            match info {
                RuntimeInfo::Reply { .. } => Self::Runtime("reply"),
                RuntimeInfo::Task(_) => Self::Runtime("task"),
                RuntimeInfo::CallTimeout(_) => Self::Runtime("timeout"),
                RuntimeInfo::Exit(_) => Self::Runtime("exit"),
                RuntimeInfo::Down(_) => Self::Runtime("down"),
                RuntimeInfo::Timer(_) => Self::Runtime("timer"),
                RuntimeInfo::System(_) => Self::Runtime("system"),
            }
        }
    }

    struct GateMachine;

    impl GenStatem for GateMachine {
        type State = GateState;
        type Data = usize;
        type Call = GateCall;
        type Cast = GateCast;
        type Reply = GateSnapshot;
        type Info = GateInfo;

        fn init<C: Context>(
            &mut self,
            _ctx: &mut C,
        ) -> Result<(Self::State, Self::Data), ExitReason> {
            Ok((GateState::Closed, 0))
        }

        fn handle_call<C: Context>(
            &mut self,
            state: &mut Self::State,
            pushes: &mut Self::Data,
            _from: ReplyToken,
            message: Self::Call,
            _ctx: &mut C,
        ) -> StatemCallOutcome<Self::State, Self::Reply> {
            match message {
                GateCall::Snapshot => StatemCallOutcome::Reply(GateSnapshot {
                    state: state.clone(),
                    pushes: *pushes,
                }),
            }
        }

        fn handle_cast<C: Context>(
            &mut self,
            state: &mut Self::State,
            pushes: &mut Self::Data,
            message: Self::Cast,
            _ctx: &mut C,
        ) -> StatemOutcome<Self::State> {
            match message {
                GateCast::Unlock if *state == GateState::Closed => {
                    StatemOutcome::Transition(GateState::Open)
                }
                GateCast::Push if *state == GateState::Open => {
                    *pushes += 1;
                    StatemOutcome::Transition(GateState::Closed)
                }
                GateCast::Unlock | GateCast::Push => StatemOutcome::Continue,
            }
        }

        fn handle_info<C: Context>(
            &mut self,
            _state: &mut Self::State,
            _pushes: &mut Self::Data,
            _message: Self::Info,
            _ctx: &mut C,
        ) -> StatemOutcome<Self::State> {
            StatemOutcome::Continue
        }
    }

    struct SnapshotClient {
        server: crate::ActorId,
        seen: Arc<Mutex<Vec<GateSnapshot>>>,
    }

    impl crate::Actor for SnapshotClient {
        fn init<C: Context>(&mut self, ctx: &mut C) -> Result<(), ExitReason> {
            ctx.ask(self.server, GateCall::Snapshot, None)
                .map_err(|error| ExitReason::Error(format!("ask failed: {error:?}")))?;
            Ok(())
        }

        fn handle<C: Context>(&mut self, envelope: Envelope, _ctx: &mut C) -> ActorTurn {
            if let Envelope::Reply { message, .. } = envelope {
                self.seen
                    .lock()
                    .unwrap()
                    .push(message.downcast::<GateSnapshot>().ok().unwrap());
            }

            ActorTurn::Continue
        }
    }

    #[test]
    fn gen_statem_actor_transitions_and_replies() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = LocalRuntime::default();
        let machine = runtime.spawn(GenStatemActor::new(GateMachine)).unwrap();

        runtime.run_until_idle();
        runtime
            .send(machine, CastMessage(GateCast::Unlock))
            .unwrap();
        runtime.send(machine, CastMessage(GateCast::Push)).unwrap();
        runtime
            .send(machine, InfoMessage(GateInfo::User("noop")))
            .unwrap();
        runtime
            .spawn(SnapshotClient {
                server: machine,
                seen: Arc::clone(&seen),
            })
            .unwrap();
        runtime.run_until_idle();

        assert_eq!(
            seen.lock().unwrap().as_slice(),
            &[GateSnapshot {
                state: GateState::Closed,
                pushes: 1,
            }]
        );
    }
}
