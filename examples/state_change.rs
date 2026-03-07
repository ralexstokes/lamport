use lamport::{
    Actor, ActorTurn, Context, ControlError, Envelope, ExitReason, LocalRuntime, Shutdown,
    StateSnapshot,
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct CounterState {
    value: i32,
    label: String,
}

struct CounterActor {
    value: i32,
    label: String,
    version: u64,
}

impl CounterActor {
    fn snapshot(&self) -> CounterState {
        CounterState {
            value: self.value,
            label: self.label.clone(),
        }
    }
}

impl Actor for CounterActor {
    fn handle<C: Context>(&mut self, envelope: Envelope, _ctx: &mut C) -> ActorTurn {
        match envelope {
            Envelope::User(payload) => match payload.downcast::<i32>() {
                Ok(delta) => {
                    self.value += delta;
                    ActorTurn::Continue
                }
                Err(payload) => ActorTurn::Stop(ExitReason::Error(format!(
                    "unexpected counter payload `{}`",
                    payload.type_name()
                ))),
            },
            other => ActorTurn::Stop(ExitReason::Error(format!(
                "unexpected counter envelope `{other:?}`"
            ))),
        }
    }

    fn state_version(&self) -> u64 {
        self.version
    }

    fn inspect_state<C: Context>(&mut self, _ctx: &mut C) -> Result<StateSnapshot, ControlError> {
        Ok(StateSnapshot::new(self.version, self.snapshot()))
    }

    fn replace_state<C: Context>(
        &mut self,
        snapshot: StateSnapshot,
        _ctx: &mut C,
    ) -> Result<(), ControlError> {
        let replacement = match snapshot.payload.downcast::<CounterState>() {
            Ok(state) => state,
            Err(payload) => {
                return Err(ControlError::invalid_state(
                    std::any::type_name::<CounterState>(),
                    payload.type_name(),
                ));
            }
        };

        self.value = replacement.value;
        self.label = replacement.label;
        Ok(())
    }

    fn code_change<C: Context>(
        &mut self,
        target_version: u64,
        _ctx: &mut C,
    ) -> Result<(), ControlError> {
        match (self.version, target_version) {
            (current, requested) if current == requested => Ok(()),
            (0, 1) => {
                self.value *= 10;
                self.label = format!("v1:{}", self.label);
                self.version = 1;
                Ok(())
            }
            (current, requested) => Err(ControlError::VersionMismatch { current, requested }),
        }
    }
}

fn read_state(snapshot: StateSnapshot) -> Result<(u64, CounterState), String> {
    let version = snapshot.version;
    let state = snapshot
        .payload
        .downcast::<CounterState>()
        .map_err(|payload| format!("unexpected snapshot payload `{}`", payload.type_name()))?;
    Ok((version, state))
}

fn print_state(
    runtime: &mut LocalRuntime,
    actor: lamport::ActorId,
    label: &str,
) -> Result<(), String> {
    let snapshot = runtime
        .get_state(actor)
        .map_err(|error| format!("get_state failed: {error}"))?;
    let (version, state) = read_state(snapshot)?;
    println!("{label}: version={version} state={state:?}");
    Ok(())
}

fn run() -> Result<(), String> {
    let mut runtime = LocalRuntime::default();
    let actor = runtime
        .spawn(CounterActor {
            value: 1,
            label: "counter".into(),
            version: 0,
        })
        .map_err(|error| format!("spawn failed: {error:?}"))?;

    print_state(&mut runtime, actor, "initial")?;

    runtime
        .send(actor, 2_i32)
        .map_err(|error| format!("send failed: {error:?}"))?;
    runtime.run_until_idle();
    print_state(&mut runtime, actor, "after user message")?;

    runtime
        .replace_state(
            actor,
            StateSnapshot::new(
                0,
                CounterState {
                    value: 7,
                    label: "replaced".into(),
                },
            ),
        )
        .map_err(|error| format!("replace_state failed: {error}"))?;
    print_state(&mut runtime, actor, "after replace_state")?;

    runtime
        .code_change_actor(actor, 1)
        .map_err(|error| format!("code_change_actor failed: {error}"))?;
    print_state(&mut runtime, actor, "after code_change_actor")?;

    runtime
        .shutdown_actor(actor, Shutdown::Infinity)
        .map_err(|error| format!("shutdown_actor failed: {error:?}"))?;
    runtime.run_until_idle();
    println!("final snapshot: {:?}", runtime.actor_snapshot(actor));

    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
