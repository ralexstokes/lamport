use crate::{
    actor::{Actor, ActorTurn},
    context::Context,
    control::{ControlError, StateSnapshot},
    envelope::{Envelope, Message, Payload, ReplyToken},
    types::ExitReason,
};

use super::{
    RuntimeInfo,
    adapter::{
        DispatchEnvelope, UserMessage, classify_envelope, downcast_payload, downcast_user_message,
        initialized_pair,
    },
};

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

    /// Returns the control-plane state version for this behaviour.
    fn state_version(&self) -> u64 {
        0
    }

    /// Returns a type-erased snapshot for `GetState`.
    fn inspect_state<C: Context>(
        &mut self,
        _state: &mut Self::State,
        _data: &mut Self::Data,
        _ctx: &mut C,
    ) -> Result<Payload, ControlError> {
        Err(ControlError::unsupported("GetState"))
    }

    /// Replaces state and data from a type-erased control payload.
    fn replace_state<C: Context>(
        &mut self,
        _state: &mut Self::State,
        _data: &mut Self::Data,
        _replacement: Payload,
        _ctx: &mut C,
    ) -> Result<(), ControlError> {
        Err(ControlError::unsupported("ReplaceState"))
    }

    /// Runs during a reserved code-change flow.
    fn code_change<C: Context>(
        &mut self,
        _state: &mut Self::State,
        _data: &mut Self::Data,
        from_version: u64,
        to_version: u64,
        _ctx: &mut C,
    ) -> Result<(), ControlError> {
        if from_version == to_version {
            Ok(())
        } else {
            Err(ControlError::VersionMismatch {
                current: from_version,
                requested: to_version,
            })
        }
    }
}

/// Actor adapter that makes a typed `GenStatem` spawnable on the raw runtime.
pub struct GenStatemActor<G: GenStatem>
where
    G::Info: From<RuntimeInfo>,
{
    machine: G,
    state: Option<G::State>,
    data: Option<G::Data>,
    state_version: u64,
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
            state_version: 0,
        }
    }

    fn machine_state_and_data_mut(
        &mut self,
    ) -> Result<(&mut G, &mut G::State, &mut G::Data), ActorTurn> {
        let Self {
            machine,
            state,
            data,
            ..
        } = self;
        let (state, data) = initialized_pair(state, data, "gen statem state", "gen statem data")?;
        Ok((machine, state, data))
    }

    fn handle_call<C: Context>(
        &mut self,
        token: ReplyToken,
        message: Payload,
        ctx: &mut C,
    ) -> ActorTurn {
        let call = match downcast_payload::<G::Call>(message, "statem call") {
            Ok(call) => call,
            Err(turn) => return turn,
        };

        let (machine, state, data) = match self.machine_state_and_data_mut() {
            Ok(parts) => parts,
            Err(turn) => return turn,
        };
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
        match downcast_user_message::<G::Cast, G::Info>(payload, "statem") {
            Ok(UserMessage::Cast(message)) => {
                let (machine, state, data) = match self.machine_state_and_data_mut() {
                    Ok(parts) => parts,
                    Err(turn) => return turn,
                };
                let outcome = machine.handle_cast(state, data, message, ctx);
                map_statem_outcome(state, outcome)
            }
            Ok(UserMessage::Info(message)) => {
                let (machine, state, data) = match self.machine_state_and_data_mut() {
                    Ok(parts) => parts,
                    Err(turn) => return turn,
                };
                let outcome = machine.handle_info(state, data, message, ctx);
                map_statem_outcome(state, outcome)
            }
            Err(turn) => turn,
        }
    }

    fn handle_runtime_info<C: Context>(&mut self, info: RuntimeInfo, ctx: &mut C) -> ActorTurn {
        let (machine, state, data) = match self.machine_state_and_data_mut() {
            Ok(parts) => parts,
            Err(turn) => return turn,
        };
        let outcome = machine.handle_info(state, data, G::Info::from(info), ctx);
        map_statem_outcome(state, outcome)
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
        self.state_version = self.machine.state_version();
        Ok(())
    }

    fn handle<C: Context>(&mut self, envelope: Envelope, ctx: &mut C) -> ActorTurn {
        match classify_envelope(envelope) {
            DispatchEnvelope::Request { token, message } => self.handle_call(token, message, ctx),
            DispatchEnvelope::User(payload) => self.handle_user(payload, ctx),
            DispatchEnvelope::Runtime(info) => self.handle_runtime_info(info, ctx),
        }
    }

    fn terminate<C: Context>(&mut self, reason: ExitReason, ctx: &mut C) {
        if let (Some(state), Some(data)) = (self.state.as_mut(), self.data.as_mut()) {
            self.machine.terminate(state, data, reason, ctx);
        }
    }

    fn state_version(&self) -> u64 {
        self.state_version
    }

    fn inspect_state<C: Context>(&mut self, ctx: &mut C) -> Result<StateSnapshot, ControlError> {
        let (machine, state, data) = self
            .machine_state_and_data_mut()
            .map_err(|_| ControlError::rejected("GetState", "state machine is not initialized"))?;
        let payload = machine.inspect_state(state, data, ctx)?;
        Ok(StateSnapshot::from_payload(self.state_version, payload))
    }

    fn replace_state<C: Context>(
        &mut self,
        snapshot: StateSnapshot,
        ctx: &mut C,
    ) -> Result<(), ControlError> {
        let (machine, state, data) = self.machine_state_and_data_mut().map_err(|_| {
            ControlError::rejected("ReplaceState", "state machine is not initialized")
        })?;
        machine.replace_state(state, data, snapshot.payload, ctx)
    }

    fn code_change<C: Context>(
        &mut self,
        target_version: u64,
        ctx: &mut C,
    ) -> Result<(), ControlError> {
        let from_version = self.state_version;
        let (machine, state, data) = self.machine_state_and_data_mut().map_err(|_| {
            ControlError::rejected("CodeChange", "state machine is not initialized")
        })?;
        machine.code_change(state, data, from_version, target_version, ctx)?;
        self.state_version = target_version;
        Ok(())
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
