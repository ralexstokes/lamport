mod adapter;
mod gen_server;
mod gen_statem;

pub use gen_server::{CallOutcome, GenServer, GenServerActor, ServerOutcome};
pub use gen_statem::{GenStatem, GenStatemActor, StatemCallOutcome, StatemOutcome};

use crate::{
    envelope::{
        CallTimedOut, DownMessage, ExitSignal, Payload, SystemMessage, TaskCompleted, TimerFired,
    },
    types::Ref,
};

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
