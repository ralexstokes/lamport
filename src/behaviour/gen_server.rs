use crate::{
    actor::{Actor, ActorTurn},
    context::Context,
    envelope::{Envelope, Message, Payload, ReplyToken},
    types::ExitReason,
};

use super::{
    RuntimeInfo,
    adapter::{
        DispatchEnvelope, UserMessage, classify_envelope, downcast_payload, downcast_user_message,
        initialized_state,
    },
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

/// Actor adapter that makes a typed `GenServer` spawnable on the raw runtime.
pub struct GenServerActor<G: GenServer>
where
    G::Info: From<RuntimeInfo>,
    G::State: Send,
{
    server: G,
    state: Option<G::State>,
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

    fn server_and_state_mut(&mut self) -> Result<(&mut G, &mut G::State), ActorTurn> {
        let Self { server, state } = self;
        let state = initialized_state(state, "gen server state")?;
        Ok((server, state))
    }

    fn handle_call<C: Context>(
        &mut self,
        token: ReplyToken,
        message: Payload,
        ctx: &mut C,
    ) -> ActorTurn {
        let call = match downcast_payload::<G::Call>(message, "call") {
            Ok(call) => call,
            Err(turn) => return turn,
        };

        let (server, state) = match self.server_and_state_mut() {
            Ok(parts) => parts,
            Err(turn) => return turn,
        };
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
        match downcast_user_message::<G::Cast, G::Info>(payload, "server") {
            Ok(UserMessage::Cast(message)) => {
                let (server, state) = match self.server_and_state_mut() {
                    Ok(parts) => parts,
                    Err(turn) => return turn,
                };
                map_server_outcome(server.handle_cast(state, message, ctx))
            }
            Ok(UserMessage::Info(message)) => {
                let (server, state) = match self.server_and_state_mut() {
                    Ok(parts) => parts,
                    Err(turn) => return turn,
                };
                map_server_outcome(server.handle_info(state, message, ctx))
            }
            Err(turn) => turn,
        }
    }

    fn handle_runtime_info<C: Context>(&mut self, info: RuntimeInfo, ctx: &mut C) -> ActorTurn {
        let (server, state) = match self.server_and_state_mut() {
            Ok(parts) => parts,
            Err(turn) => return turn,
        };
        map_server_outcome(server.handle_info(state, G::Info::from(info), ctx))
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
        match classify_envelope(envelope) {
            DispatchEnvelope::Request { token, message } => self.handle_call(token, message, ctx),
            DispatchEnvelope::User(payload) => self.handle_user(payload, ctx),
            DispatchEnvelope::Runtime(info) => self.handle_runtime_info(info, ctx),
        }
    }

    fn terminate<C: Context>(&mut self, reason: ExitReason, ctx: &mut C) {
        if let Some(state) = self.state.as_mut() {
            self.server.terminate(state, reason, ctx);
        }
    }
}

fn map_server_outcome(outcome: ServerOutcome) -> ActorTurn {
    match outcome {
        ServerOutcome::Continue => ActorTurn::Continue,
        ServerOutcome::Yield => ActorTurn::Yield,
        ServerOutcome::Stop(reason) => ActorTurn::Stop(reason),
    }
}
