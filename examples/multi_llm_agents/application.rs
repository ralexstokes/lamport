use std::collections::BTreeMap;

use lamport::{
    ActorId, Application, CallOutcome, ChildSpec, Context, ExitReason, GenServer, Ref, ReplyToken,
    Restart, ServerOutcome, Shutdown, SpawnOptions, StartChildError, Strategy, Supervisor,
    SupervisorDirective, SupervisorFlags, behaviour::RuntimeInfo, restart_scope,
};

use super::{
    COORDINATOR_CHILD, COORDINATOR_NAME, PROVIDER_REQUEST_TIMEOUT, ROOT_NAME,
    game::{Board, GameOutcome, Mark, TurnRecord, parse_move, random_x_player},
    provider::{
        ExampleConfig, ProviderAnswer, ProviderCall, ProviderKind, ProviderReply, ProviderServer,
    },
};

#[derive(Debug, Default)]
struct CoordinatorState {
    pending_calls: BTreeMap<Ref, ProviderKind>,
    monitors: BTreeMap<Ref, ProviderKind>,
    last_seen_turns: BTreeMap<ProviderKind, usize>,
    board: Board,
    turns: Vec<TurnRecord>,
    outcome: Option<GameOutcome>,
}

struct CoordinatorServer {
    models: BTreeMap<ProviderKind, String>,
    x_player: ProviderKind,
}

impl CoordinatorServer {
    fn new(config: &ExampleConfig) -> Self {
        Self {
            models: ProviderKind::ALL
                .into_iter()
                .map(|provider| {
                    let model = match provider {
                        ProviderKind::OpenAI => config.openai.model.clone(),
                        ProviderKind::Anthropic => config.anthropic.model.clone(),
                    };
                    (provider, model)
                })
                .collect(),
            x_player: random_x_player(),
        }
    }

    fn model(&self, provider: ProviderKind) -> &str {
        self.models
            .get(&provider)
            .map(String::as_str)
            .unwrap_or("unknown-model")
    }

    fn mark(&self, provider: ProviderKind) -> Mark {
        if provider == self.x_player {
            Mark::X
        } else {
            Mark::O
        }
    }

    fn next_player(&self, state: &CoordinatorState) -> ProviderKind {
        if state.turns.len().is_multiple_of(2) {
            self.x_player
        } else {
            self.x_player.opponent()
        }
    }

    fn system_prompt(&self, provider: ProviderKind) -> String {
        format!(
            "You are playing tic-tac-toe against another LLM.\n\
             Your mark is {provider_mark} and your opponent is {opponent_mark}.\n\
             The current board in the latest user message is authoritative.\n\
             Choose the strongest legal move each turn.\n\
             Respond with JSON exactly matching this shape: {{\"move\": <square-number>}}.\n\
             The move must be a legal square number from 1 to 9.\n\
             Do not include any keys other than `move`, and do not include explanations or markdown.\n\
             If you return anything except one legal move, you lose by forfeit.",
            provider_mark = self.mark(provider).symbol(),
            opponent_mark = self.mark(provider.opponent()).symbol(),
        )
    }

    fn turn_update(&self, state: &CoordinatorState, provider: ProviderKind) -> String {
        let last_seen_turn = state.last_seen_turns.get(&provider).copied().unwrap_or(0);
        let updates = state
            .turns
            .iter()
            .filter(|turn| turn.number > last_seen_turn)
            .map(|turn| {
                format!(
                    "{}. {} ({}) played square {}",
                    turn.number,
                    turn.player.label(),
                    self.mark(turn.player).symbol(),
                    turn.square
                )
            })
            .collect::<Vec<_>>();

        let delta = if updates.is_empty() {
            if state.turns.is_empty() {
                "No moves have been played yet.".to_owned()
            } else {
                "No new moves since your last response.".to_owned()
            }
        } else {
            updates.join("\n")
        };

        format!(
            "Match update for turn {turn_number}.\n\
             New moves since your last response:\n\
             {delta}\n\n\
             Current board:\n\
             {board}\n\n\
             Available squares: {available}\n\n\
             Choose your next move now.",
            turn_number = state.turns.len() + 1,
            board = state
                .board
                .render_for_prompt(|player| self.mark(player).symbol()),
            available = state.board.available_squares(),
        )
    }

    fn provider_outcome(&self, provider: ProviderKind, detail: String) -> GameOutcome {
        GameOutcome::Win {
            winner: provider.opponent(),
            detail,
        }
    }

    fn start_turn<C: Context>(
        &self,
        state: &mut CoordinatorState,
        ctx: &mut C,
    ) -> Result<(), ExitReason> {
        let provider = self.next_player(state);
        let actor = ctx.whereis(provider.registered_name()).ok_or_else(|| {
            ExitReason::Error(format!(
                "{} provider `{}` is not registered",
                provider.label(),
                provider.registered_name()
            ))
        })?;

        println!(
            "Turn {}: asking {} ({}) [{}] for a move",
            state.turns.len() + 1,
            provider.label(),
            self.mark(provider).symbol(),
            self.model(provider)
        );

        let call = ctx
            .ask(
                actor,
                ProviderCall::ChooseMove {
                    system_prompt: self.system_prompt(provider),
                    update: self.turn_update(state, provider),
                },
                Some(PROVIDER_REQUEST_TIMEOUT),
            )
            .map_err(|error| {
                ExitReason::Error(format!(
                    "ask {} provider failed: {error:?}",
                    provider.label()
                ))
            })?;
        state.pending_calls.insert(call.reference, provider);
        Ok(())
    }

    fn finish_game(&self, state: &mut CoordinatorState, outcome: GameOutcome) -> ServerOutcome {
        state.outcome = Some(outcome);
        self.print_summary(state);
        ServerOutcome::Stop(ExitReason::Normal)
    }

    fn handle_provider_reply<C: Context>(
        &self,
        state: &mut CoordinatorState,
        provider: ProviderKind,
        reply: ProviderReply,
        ctx: &mut C,
    ) -> ServerOutcome {
        match reply {
            ProviderReply::Success(answer) => self.handle_move_answer(state, provider, answer, ctx),
            ProviderReply::Failure(failure) => self.finish_game(
                state,
                self.provider_outcome(
                    provider,
                    format!(
                        "{} ({}) forfeited after provider request failure from model `{}`: {}",
                        provider.label(),
                        self.mark(provider).symbol(),
                        failure.model,
                        failure.error
                    ),
                ),
            ),
        }
    }

    fn handle_move_answer<C: Context>(
        &self,
        state: &mut CoordinatorState,
        provider: ProviderKind,
        answer: ProviderAnswer,
        ctx: &mut C,
    ) -> ServerOutcome {
        let square = match parse_move(&answer.text) {
            Ok(square) => square,
            Err(error) => {
                return self.finish_game(
                    state,
                    self.provider_outcome(
                        provider,
                        format!(
                            "{} ({}) forfeited because its response was not a valid square: {}. Raw response: {}",
                            provider.label(),
                            self.mark(provider).symbol(),
                            error,
                            answer.text
                        ),
                    ),
                );
            }
        };

        if let Err(error) = state.board.apply_move(provider, square) {
            return self.finish_game(
                state,
                self.provider_outcome(
                    provider,
                    format!(
                        "{} ({}) forfeited with illegal move {}: {}. Raw response: {}",
                        provider.label(),
                        self.mark(provider).symbol(),
                        square,
                        error,
                        answer.text
                    ),
                ),
            );
        }

        let turn = TurnRecord {
            number: state.turns.len() + 1,
            player: provider,
            model: answer.model,
            square,
            raw_response: answer.text,
            cache_summary: answer.cache_summary,
        };
        state.turns.push(turn.clone());
        state.last_seen_turns.insert(provider, turn.number);

        println!(
            "Turn {}: {} ({}) [{}] played square {}\n{}\n",
            turn.number,
            provider.label(),
            self.mark(provider).symbol(),
            turn.model,
            turn.square,
            state
                .board
                .render_final(|player| self.mark(player).symbol()),
        );
        if let Some(cache_summary) = turn.cache_summary.as_deref() {
            println!("Cache: {cache_summary}\n");
        }

        if state.board.winner() == Some(provider) {
            return self.finish_game(
                state,
                GameOutcome::Win {
                    winner: provider,
                    detail: format!(
                        "{} ({}) completed three in a row",
                        provider.label(),
                        self.mark(provider).symbol()
                    ),
                },
            );
        }

        if state.board.is_full() {
            return self.finish_game(state, GameOutcome::Draw);
        }

        if let Err(reason) = self.start_turn(state, ctx) {
            return ServerOutcome::Stop(reason);
        }

        ServerOutcome::Continue
    }

    fn print_summary(&self, state: &CoordinatorState) {
        println!(
            "Final board:\n{}\n",
            state
                .board
                .render_final(|player| self.mark(player).symbol())
        );
        println!("Move log:");

        if state.turns.is_empty() {
            println!("(no completed moves)\n");
        } else {
            for turn in &state.turns {
                println!(
                    "{}. {} ({}) [{}] -> {}",
                    turn.number,
                    turn.player.label(),
                    self.mark(turn.player).symbol(),
                    turn.model,
                    turn.square
                );
                if let Some(cache_summary) = turn.cache_summary.as_deref() {
                    println!("   cache: {cache_summary}");
                }

                let raw = turn.raw_response.trim();
                if raw != turn.square.to_string() {
                    println!("   raw response:\n{}", turn.raw_response);
                }
            }
            println!();
        }

        match state.outcome.as_ref() {
            Some(GameOutcome::Win { winner, detail }) => {
                println!(
                    "Result: {} ({}) wins.\n{}\n",
                    winner.label(),
                    self.mark(*winner).symbol(),
                    detail
                );
            }
            Some(GameOutcome::Draw) => {
                println!("Result: draw.\nNo legal moves remain.\n");
            }
            None => {
                println!("Result: unfinished.\n");
            }
        }
    }
}

impl GenServer for CoordinatorServer {
    type State = CoordinatorState;
    type Call = ();
    type Cast = ();
    type Reply = ();
    type Info = RuntimeInfo;

    fn init<C: Context>(&mut self, ctx: &mut C) -> Result<Self::State, ExitReason> {
        let mut state = CoordinatorState::default();

        for provider in ProviderKind::ALL {
            let actor = ctx.whereis(provider.registered_name()).ok_or_else(|| {
                ExitReason::Error(format!(
                    "{} provider `{}` is not registered",
                    provider.label(),
                    provider.registered_name()
                ))
            })?;

            let monitor = ctx.monitor(actor).map_err(|error| {
                ExitReason::Error(format!(
                    "monitor {} provider failed: {error:?}",
                    provider.label()
                ))
            })?;
            state.monitors.insert(monitor, provider);
        }

        println!(
            "Matchup: {} [{}] is X, {} [{}] is O\n",
            self.x_player.label(),
            self.model(self.x_player),
            self.x_player.opponent().label(),
            self.model(self.x_player.opponent()),
        );
        println!(
            "Initial board:\n{}\n",
            state
                .board
                .render_for_prompt(|player| self.mark(player).symbol())
        );

        self.start_turn(&mut state, ctx)?;
        Ok(state)
    }

    fn handle_call<C: Context>(
        &mut self,
        _state: &mut Self::State,
        _from: ReplyToken,
        _message: Self::Call,
        _ctx: &mut C,
    ) -> CallOutcome<Self::Reply> {
        CallOutcome::Reply(())
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
        state: &mut Self::State,
        message: Self::Info,
        ctx: &mut C,
    ) -> ServerOutcome {
        match message {
            RuntimeInfo::Reply { reference, message } => {
                let Some(provider) = state.pending_calls.remove(&reference) else {
                    return ServerOutcome::Continue;
                };

                let reply = match message.downcast::<ProviderReply>() {
                    Ok(reply) => reply,
                    Err(payload) => {
                        let payload_type = payload.type_name();
                        return ServerOutcome::Stop(ExitReason::Error(format!(
                            "unexpected coordinator reply `{payload_type}`",
                        )));
                    }
                };

                self.handle_provider_reply(state, provider, reply, ctx)
            }
            RuntimeInfo::CallTimeout(timeout) => {
                let Some(provider) = state.pending_calls.remove(&timeout.reference) else {
                    return ServerOutcome::Continue;
                };

                self.finish_game(
                    state,
                    self.provider_outcome(
                        provider,
                        format!(
                            "{} ({}) forfeited after timing out waiting for a move after {:?}",
                            provider.label(),
                            self.mark(provider).symbol(),
                            PROVIDER_REQUEST_TIMEOUT
                        ),
                    ),
                )
            }
            RuntimeInfo::Down(message) => {
                let Some(provider) = state.monitors.remove(&message.reference) else {
                    return ServerOutcome::Continue;
                };

                self.finish_game(
                    state,
                    self.provider_outcome(
                        provider,
                        format!(
                            "{} ({}) became unavailable before the game finished: actor {} exited with {}",
                            provider.label(),
                            self.mark(provider).symbol(),
                            message.actor,
                            message.reason
                        ),
                    ),
                )
            }
            _ => ServerOutcome::Continue,
        }
    }
}

pub(super) struct MultiLlmSupervisor {
    flags: SupervisorFlags,
    specs: Vec<ChildSpec>,
    config: ExampleConfig,
}

impl MultiLlmSupervisor {
    fn new(config: ExampleConfig) -> Self {
        Self {
            flags: SupervisorFlags {
                strategy: Strategy::OneForOne,
                ..SupervisorFlags::default()
            },
            specs: vec![
                ChildSpec {
                    id: ProviderKind::OpenAI.child_id(),
                    restart: Restart::Permanent,
                    shutdown: Shutdown::default(),
                    is_supervisor: false,
                },
                ChildSpec {
                    id: ProviderKind::Anthropic.child_id(),
                    restart: Restart::Permanent,
                    shutdown: Shutdown::default(),
                    is_supervisor: false,
                },
                ChildSpec {
                    id: COORDINATOR_CHILD,
                    restart: Restart::Temporary,
                    shutdown: Shutdown::default(),
                    is_supervisor: false,
                },
            ],
            config,
        }
    }

    fn spawn_provider<C: Context>(
        &mut self,
        ctx: &mut C,
        config: super::provider::ProviderConfig,
        provider: ProviderKind,
    ) -> Result<ActorId, StartChildError> {
        ctx.spawn_gen_server(
            ProviderServer::new(config),
            SpawnOptions {
                registered_name: Some(provider.registered_name().into()),
                ..SpawnOptions::default()
            },
        )
        .map_err(|_| StartChildError::SpawnRejected)
    }

    fn spawn_coordinator<C: Context>(&mut self, ctx: &mut C) -> Result<ActorId, StartChildError> {
        ctx.spawn_gen_server(
            CoordinatorServer::new(&self.config),
            SpawnOptions {
                registered_name: Some(COORDINATOR_NAME.into()),
                ..SpawnOptions::default()
            },
        )
        .map_err(|_| StartChildError::SpawnRejected)
    }
}

impl Supervisor for MultiLlmSupervisor {
    fn flags(&self) -> SupervisorFlags {
        self.flags
    }

    fn child_specs(&self) -> &[ChildSpec] {
        &self.specs
    }

    fn start_child<C: Context>(
        &mut self,
        spec: &ChildSpec,
        ctx: &mut C,
    ) -> Result<ActorId, StartChildError> {
        match spec.id {
            id if id == ProviderKind::OpenAI.child_id() => {
                self.spawn_provider(ctx, self.config.openai.clone(), ProviderKind::OpenAI)
            }
            id if id == ProviderKind::Anthropic.child_id() => {
                self.spawn_provider(ctx, self.config.anthropic.clone(), ProviderKind::Anthropic)
            }
            COORDINATOR_CHILD => self.spawn_coordinator(ctx),
            other => Err(StartChildError::InitFailed(ExitReason::Error(format!(
                "unknown child `{other}`",
            )))),
        }
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

pub(super) struct MultiLlmApplication {
    pub(super) config: ExampleConfig,
}

impl Application for MultiLlmApplication {
    type RootSupervisor = MultiLlmSupervisor;

    fn name(&self) -> &'static str {
        "multi-llm-demo"
    }

    fn root_options(&self) -> SpawnOptions {
        SpawnOptions {
            registered_name: Some(ROOT_NAME.into()),
            ..SpawnOptions::default()
        }
    }

    fn root_supervisor(self) -> Self::RootSupervisor {
        MultiLlmSupervisor::new(self.config)
    }
}
