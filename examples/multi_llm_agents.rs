use std::{
    collections::{BTreeMap, hash_map::RandomState},
    env,
    hash::{BuildHasher, Hasher},
    time::{Duration, Instant},
};

use lamport::{
    ActorId, ActorStatus, Application, CallOutcome, ChildSpec, Context, ExitReason, GenServer,
    LocalRuntime, Ref, ReplyToken, Restart, RuntimeInfo, ServerOutcome, Shutdown, SpawnOptions,
    StartChildError, Strategy, Supervisor, SupervisorDirective, SupervisorFlags,
    boot_local_application, restart_scope,
};
use reqwest::blocking::Client;
use serde::Deserialize;
use serde_json::{Value, json};

const DEFAULT_OPENAI_MODEL: &str = "gpt-5-mini-2025-08-07";
const DEFAULT_ANTHROPIC_MODEL: &str = "claude-sonnet-4-6";

const ROOT_NAME: &str = "multi-llm.root";
const COORDINATOR_CHILD: &str = "coordinator";
const COORDINATOR_NAME: &str = "multi-llm.coordinator";
const PROVIDER_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const COORDINATOR_DEADLINE: Duration = Duration::from_secs(180);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ProviderKind {
    OpenAI,
    Anthropic,
}

impl ProviderKind {
    const ALL: [Self; 2] = [Self::OpenAI, Self::Anthropic];

    fn label(self) -> &'static str {
        match self {
            Self::OpenAI => "openai",
            Self::Anthropic => "anthropic",
        }
    }

    fn child_id(self) -> &'static str {
        match self {
            Self::OpenAI => "openai-provider",
            Self::Anthropic => "anthropic-provider",
        }
    }

    fn registered_name(self) -> &'static str {
        match self {
            Self::OpenAI => "openai-agent",
            Self::Anthropic => "anthropic-agent",
        }
    }

    fn opponent(self) -> Self {
        match self {
            Self::OpenAI => Self::Anthropic,
            Self::Anthropic => Self::OpenAI,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mark {
    X,
    O,
}

impl Mark {
    fn symbol(self) -> &'static str {
        match self {
            Self::X => "X",
            Self::O => "O",
        }
    }
}

#[derive(Debug, Clone)]
struct ProviderConfig {
    kind: ProviderKind,
    model: String,
    api_key: String,
}

#[derive(Debug, Clone)]
struct ExampleConfig {
    openai: ProviderConfig,
    anthropic: ProviderConfig,
}

impl ExampleConfig {
    fn from_env() -> Result<Self, String> {
        let openai_api_key = env::var("OPENAI_API_KEY")
            .map_err(|_| "missing OPENAI_API_KEY in the environment".to_owned())?;
        let anthropic_api_key = env::var("ANTHROPIC_API_KEY")
            .map_err(|_| "missing ANTHROPIC_API_KEY in the environment".to_owned())?;

        Ok(Self {
            openai: ProviderConfig {
                kind: ProviderKind::OpenAI,
                model: env::var("OPENAI_MODEL").unwrap_or_else(|_| DEFAULT_OPENAI_MODEL.to_owned()),
                api_key: openai_api_key,
            },
            anthropic: ProviderConfig {
                kind: ProviderKind::Anthropic,
                model: env::var("ANTHROPIC_MODEL")
                    .unwrap_or_else(|_| DEFAULT_ANTHROPIC_MODEL.to_owned()),
                api_key: anthropic_api_key,
            },
        })
    }
}

#[derive(Debug, Clone)]
enum ProviderCall {
    ChooseMove { prompt: String },
}

#[derive(Debug, Clone)]
enum ProviderReply {
    Success(ProviderAnswer),
    Failure(ProviderFailure),
}

#[derive(Debug, Clone)]
struct ProviderAnswer {
    model: String,
    text: String,
}

#[derive(Debug, Clone)]
struct ProviderFailure {
    model: String,
    error: String,
}

#[derive(Debug, Default)]
struct ProviderState {
    pending: BTreeMap<Ref, ReplyToken>,
}

struct ProviderServer {
    config: ProviderConfig,
}

impl ProviderServer {
    fn new(config: ProviderConfig) -> Self {
        Self { config }
    }
}

impl GenServer for ProviderServer {
    type State = ProviderState;
    type Call = ProviderCall;
    type Cast = ();
    type Reply = ProviderReply;
    type Info = RuntimeInfo;

    fn init<C: Context>(&mut self, _ctx: &mut C) -> Result<Self::State, ExitReason> {
        Ok(ProviderState::default())
    }

    fn handle_call<C: Context>(
        &mut self,
        state: &mut Self::State,
        from: ReplyToken,
        message: Self::Call,
        ctx: &mut C,
    ) -> CallOutcome<Self::Reply> {
        match message {
            ProviderCall::ChooseMove { prompt } => {
                let config = self.config.clone();
                let task = ctx.spawn_blocking_io(move || request_provider(&config, &prompt));
                state.pending.insert(task.id(), from);
                CallOutcome::NoReply
            }
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
        state: &mut Self::State,
        message: Self::Info,
        ctx: &mut C,
    ) -> ServerOutcome {
        match message {
            RuntimeInfo::Task(task) => {
                let Some(reply_to) = state.pending.remove(&task.reference) else {
                    return ServerOutcome::Continue;
                };

                let result = match task.result.downcast::<Result<String, String>>() {
                    Ok(result) => result,
                    Err(payload) => {
                        return ServerOutcome::Stop(ExitReason::Error(format!(
                            "unexpected provider task result `{}`",
                            payload.type_name()
                        )));
                    }
                };

                let reply = match result {
                    Ok(text) => ProviderReply::Success(ProviderAnswer {
                        model: self.config.model.clone(),
                        text,
                    }),
                    Err(error) => ProviderReply::Failure(ProviderFailure {
                        model: self.config.model.clone(),
                        error,
                    }),
                };

                let _ = ctx.reply(reply_to, reply);
                ServerOutcome::Continue
            }
            _ => ServerOutcome::Continue,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct Board {
    cells: [Option<ProviderKind>; 9],
}

impl Board {
    fn render_for_prompt<F>(&self, mark_for: F) -> String
    where
        F: Fn(ProviderKind) -> &'static str,
    {
        self.render(|index, occupant| match occupant {
            Some(provider) => mark_for(provider).to_owned(),
            None => (index + 1).to_string(),
        })
    }

    fn render_final<F>(&self, mark_for: F) -> String
    where
        F: Fn(ProviderKind) -> &'static str,
    {
        self.render(|_, occupant| match occupant {
            Some(provider) => mark_for(provider).to_owned(),
            None => ".".to_owned(),
        })
    }

    fn available_squares(&self) -> String {
        self.cells
            .iter()
            .enumerate()
            .filter(|(_, occupant)| occupant.is_none())
            .map(|(index, _)| (index + 1).to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn apply_move(&mut self, provider: ProviderKind, square: usize) -> Result<(), String> {
        if !(1..=9).contains(&square) {
            return Err(format!("square {square} is outside the valid range 1-9"));
        }

        let index = square - 1;
        if let Some(occupant) = self.cells[index] {
            return Err(format!(
                "square {square} is already occupied by {}",
                occupant.label()
            ));
        }

        self.cells[index] = Some(provider);
        Ok(())
    }

    fn winner(&self) -> Option<ProviderKind> {
        const LINES: [[usize; 3]; 8] = [
            [0, 1, 2],
            [3, 4, 5],
            [6, 7, 8],
            [0, 3, 6],
            [1, 4, 7],
            [2, 5, 8],
            [0, 4, 8],
            [2, 4, 6],
        ];

        for [a, b, c] in LINES {
            let Some(provider) = self.cells[a] else {
                continue;
            };

            if self.cells[b] == Some(provider) && self.cells[c] == Some(provider) {
                return Some(provider);
            }
        }

        None
    }

    fn is_full(&self) -> bool {
        self.cells.iter().all(Option::is_some)
    }

    fn render<F>(&self, tile: F) -> String
    where
        F: Fn(usize, Option<ProviderKind>) -> String,
    {
        let mut rows = Vec::new();
        for row in 0..3 {
            let base = row * 3;
            rows.push(format!(
                " {} | {} | {} ",
                tile(base, self.cells[base]),
                tile(base + 1, self.cells[base + 1]),
                tile(base + 2, self.cells[base + 2]),
            ));
        }

        rows.join("\n---+---+---\n")
    }
}

#[derive(Debug, Clone)]
struct TurnRecord {
    number: usize,
    player: ProviderKind,
    model: String,
    square: usize,
    raw_response: String,
}

#[derive(Debug, Clone)]
enum GameOutcome {
    Win {
        winner: ProviderKind,
        detail: String,
    },
    Draw,
}

#[derive(Debug, Default)]
struct CoordinatorState {
    pending_calls: BTreeMap<Ref, ProviderKind>,
    monitors: BTreeMap<Ref, ProviderKind>,
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
            .expect("model should exist for each configured provider")
    }

    fn mark(&self, provider: ProviderKind) -> Mark {
        if provider == self.x_player {
            Mark::X
        } else {
            Mark::O
        }
    }

    fn next_player(&self, state: &CoordinatorState) -> ProviderKind {
        if state.turns.len() % 2 == 0 {
            self.x_player
        } else {
            self.x_player.opponent()
        }
    }

    fn build_turn_prompt(&self, state: &CoordinatorState, provider: ProviderKind) -> String {
        let history = if state.turns.is_empty() {
            "None yet.".to_owned()
        } else {
            state
                .turns
                .iter()
                .map(|turn| {
                    format!(
                        "{}. {} ({}) played square {}",
                        turn.number,
                        turn.player.label(),
                        self.mark(turn.player).symbol(),
                        turn.square
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        format!(
            "You are playing tic-tac-toe against another LLM.\n\
             You are the {provider_name} agent and your mark is {provider_mark}.\n\
             Your opponent is the {opponent_name} agent with mark {opponent_mark}.\n\
             For this match, {x_name} is X and {o_name} is O.\n\n\
             Current board:\n\
             {board}\n\n\
             Available squares: {available}\n\n\
             Move history:\n\
             {history}\n\n\
             Choose the strongest legal move.\n\
             Respond with JSON exactly matching this shape: {{\"move\": <square-number>}}.\n\
             The move must be a legal square number from 1 to 9.\n\
             Do not include any keys other than `move`, and do not include explanations or markdown.\n\
             If you return anything except one legal move, you lose by forfeit.",
            provider_name = provider.label(),
            provider_mark = self.mark(provider).symbol(),
            opponent_name = provider.opponent().label(),
            opponent_mark = self.mark(provider.opponent()).symbol(),
            x_name = self.x_player.label(),
            o_name = self.x_player.opponent().label(),
            board = state
                .board
                .render_for_prompt(|player| self.mark(player).symbol()),
            available = state.board.available_squares(),
            history = history,
        )
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
                    prompt: self.build_turn_prompt(state, provider),
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
                GameOutcome::Win {
                    winner: provider.opponent(),
                    detail: format!(
                        "{} ({}) forfeited after provider request failure from model `{}`: {}",
                        provider.label(),
                        self.mark(provider).symbol(),
                        failure.model,
                        failure.error
                    ),
                },
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
                    GameOutcome::Win {
                        winner: provider.opponent(),
                        detail: format!(
                            "{} ({}) forfeited because its response was not a valid square: {}. Raw response: {}",
                            provider.label(),
                            self.mark(provider).symbol(),
                            error,
                            answer.text
                        ),
                    },
                );
            }
        };

        if let Err(error) = state.board.apply_move(provider, square) {
            return self.finish_game(
                state,
                GameOutcome::Win {
                    winner: provider.opponent(),
                    detail: format!(
                        "{} ({}) forfeited with illegal move {}: {}. Raw response: {}",
                        provider.label(),
                        self.mark(provider).symbol(),
                        square,
                        error,
                        answer.text
                    ),
                },
            );
        }

        let turn = TurnRecord {
            number: state.turns.len() + 1,
            player: provider,
            model: answer.model,
            square,
            raw_response: answer.text,
        };
        state.turns.push(turn.clone());

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
                        return ServerOutcome::Stop(ExitReason::Error(format!(
                            "unexpected coordinator reply `{}`",
                            payload.type_name()
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
                    GameOutcome::Win {
                        winner: provider.opponent(),
                        detail: format!(
                            "{} ({}) forfeited after timing out waiting for a move after {:?}",
                            provider.label(),
                            self.mark(provider).symbol(),
                            PROVIDER_REQUEST_TIMEOUT
                        ),
                    },
                )
            }
            RuntimeInfo::Down(message) => {
                let Some(provider) = state.monitors.remove(&message.reference) else {
                    return ServerOutcome::Continue;
                };

                self.finish_game(
                    state,
                    GameOutcome::Win {
                        winner: provider.opponent(),
                        detail: format!(
                            "{} ({}) became unavailable before the game finished: actor {} exited with {}",
                            provider.label(),
                            self.mark(provider).symbol(),
                            message.actor,
                            message.reason
                        ),
                    },
                )
            }
            _ => ServerOutcome::Continue,
        }
    }
}

struct MultiLlmSupervisor {
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
}

impl Supervisor for MultiLlmSupervisor {
    fn flags(&self) -> SupervisorFlags {
        self.flags.clone()
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
            id if id == ProviderKind::OpenAI.child_id() => ctx
                .spawn_gen_server(
                    ProviderServer::new(self.config.openai.clone()),
                    SpawnOptions {
                        registered_name: Some(ProviderKind::OpenAI.registered_name().into()),
                        ..SpawnOptions::default()
                    },
                )
                .map_err(|_| StartChildError::SpawnRejected),
            id if id == ProviderKind::Anthropic.child_id() => ctx
                .spawn_gen_server(
                    ProviderServer::new(self.config.anthropic.clone()),
                    SpawnOptions {
                        registered_name: Some(ProviderKind::Anthropic.registered_name().into()),
                        ..SpawnOptions::default()
                    },
                )
                .map_err(|_| StartChildError::SpawnRejected),
            COORDINATOR_CHILD => ctx
                .spawn_gen_server(
                    CoordinatorServer::new(&self.config),
                    SpawnOptions {
                        registered_name: Some(COORDINATOR_NAME.into()),
                        ..SpawnOptions::default()
                    },
                )
                .map_err(|_| StartChildError::SpawnRejected),
            other => Err(StartChildError::InitFailed(ExitReason::Error(format!(
                "unknown child `{other}`"
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

struct MultiLlmApplication {
    config: ExampleConfig,
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

fn wait_until_dead(runtime: &mut LocalRuntime, actor: ActorId) {
    let deadline = Instant::now() + COORDINATOR_DEADLINE;

    while Instant::now() < deadline {
        if runtime
            .actor_snapshot(actor)
            .is_some_and(|snapshot| snapshot.status == ActorStatus::Dead)
        {
            return;
        }

        let _ = runtime.block_on_next(Some(Duration::from_millis(250)));
        runtime.run_until_idle();
    }

    panic!("actor {actor} did not terminate in time");
}

fn request_provider(config: &ProviderConfig, prompt: &str) -> Result<String, String> {
    match config.kind {
        ProviderKind::OpenAI => request_openai(config, prompt),
        ProviderKind::Anthropic => request_anthropic(config, prompt),
    }
}

fn request_openai(config: &ProviderConfig, prompt: &str) -> Result<String, String> {
    let client = Client::builder()
        .timeout(Duration::from_secs(90))
        .build()
        .map_err(|error| format!("build OpenAI client failed: {error}"))?;

    let response = client
        .post("https://api.openai.com/v1/responses")
        .bearer_auth(&config.api_key)
        .json(&json!({
            "model": config.model,
            "input": prompt,
            "max_output_tokens": 512,
            "reasoning": {
                "effort": "minimal"
            },
            "text": {
                "verbosity": "low"
            }
        }))
        .send()
        .map_err(|error| format!("OpenAI request failed: {error}"))?;

    let status = response.status();
    let body: Value = response
        .json()
        .map_err(|error| format!("decode OpenAI response failed: {error}"))?;

    if !status.is_success() {
        return Err(format_http_error("OpenAI", status.as_u16(), &body));
    }

    if let Some(reason) = openai_incomplete_reason(&body) {
        return Err(format!(
            "OpenAI response was incomplete before producing visible output: {reason}. Body: {body}"
        ));
    }

    extract_openai_text(&body)
        .ok_or_else(|| format!("OpenAI response did not contain text: {}", body))
}

fn request_anthropic(config: &ProviderConfig, prompt: &str) -> Result<String, String> {
    let client = Client::builder()
        .timeout(Duration::from_secs(90))
        .build()
        .map_err(|error| format!("build Anthropic client failed: {error}"))?;

    let response = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", &config.api_key)
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": config.model,
            "max_tokens": 128,
            "temperature": 0.0,
            "messages": [
                {
                    "role": "user",
                    "content": prompt
                }
            ],
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "schema": {
                        "type": "object",
                        "properties": {
                            "move": {
                                "type": "integer",
                                "enum": [1, 2, 3, 4, 5, 6, 7, 8, 9],
                                "description": "The legal tic-tac-toe square to play."
                            }
                        },
                        "required": ["move"],
                        "additionalProperties": false
                    }
                }
            }
        }))
        .send()
        .map_err(|error| format!("Anthropic request failed: {error}"))?;

    let status = response.status();
    let body: Value = response
        .json()
        .map_err(|error| format!("decode Anthropic response failed: {error}"))?;

    if !status.is_success() {
        return Err(format_http_error("Anthropic", status.as_u16(), &body));
    }

    extract_anthropic_text(&body)
        .ok_or_else(|| format!("Anthropic response did not contain text: {}", body))
}

fn extract_openai_text(body: &Value) -> Option<String> {
    if let Some(text) = body.get("output_text").and_then(Value::as_str) {
        let text = text.trim();
        if !text.is_empty() {
            return Some(text.to_owned());
        }
    }

    let mut parts = Vec::new();
    for item in body.get("output")?.as_array()? {
        let Some(content) = item.get("content").and_then(Value::as_array) else {
            continue;
        };

        for block in content {
            let text = block.get("text").and_then(Value::as_str);
            if let Some(text) = text.map(str::trim).filter(|text| !text.is_empty()) {
                parts.push(text.to_owned());
            }
        }
    }

    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

fn openai_incomplete_reason(body: &Value) -> Option<&str> {
    if body.get("status").and_then(Value::as_str)? != "incomplete" {
        return None;
    }

    body.get("incomplete_details")
        .and_then(|details| details.get("reason"))
        .and_then(Value::as_str)
}

fn extract_anthropic_text(body: &Value) -> Option<String> {
    let mut parts = Vec::new();
    for block in body.get("content")?.as_array()? {
        let Some(text) = block.get("text").and_then(Value::as_str) else {
            continue;
        };

        let text = text.trim();
        if !text.is_empty() {
            parts.push(text.to_owned());
        }
    }

    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

#[derive(Debug, Deserialize)]
struct JsonMove {
    #[serde(default)]
    r#move: Option<usize>,
    #[serde(default)]
    square: Option<usize>,
    #[serde(default)]
    position: Option<usize>,
    #[serde(default)]
    cell: Option<usize>,
}

fn parse_move(text: &str) -> Result<usize, String> {
    if let Some(square) = parse_move_token(text) {
        return Ok(square);
    }

    if let Some(square) = parse_json_move(text) {
        return Ok(square);
    }

    if let Some(inner) = strip_code_fence(text) {
        if let Some(square) = parse_move_token(inner) {
            return Ok(square);
        }

        if let Some(square) = parse_json_move(inner) {
            return Ok(square);
        }
    }

    Err(format!(
        "expected a single square number from 1 to 9, got `{}`",
        text
    ))
}

fn parse_move_token(text: &str) -> Option<usize> {
    let token = text
        .trim()
        .trim_matches(|ch: char| ch.is_whitespace() || matches!(ch, '`' | '"' | '\''));
    let square = token.parse::<usize>().ok()?;
    (1..=9).contains(&square).then_some(square)
}

fn parse_json_move(text: &str) -> Option<usize> {
    let move_reply = serde_json::from_str::<JsonMove>(text.trim()).ok()?;
    [
        move_reply.r#move,
        move_reply.square,
        move_reply.position,
        move_reply.cell,
    ]
    .into_iter()
    .flatten()
    .find(|square| (1..=9).contains(square))
}

fn strip_code_fence(text: &str) -> Option<&str> {
    let trimmed = text.trim();
    let rest = trimmed.strip_prefix("```")?;
    let newline = rest.find('\n')?;
    let rest = &rest[newline + 1..];
    let suffix = rest.rfind("```")?;
    Some(rest[..suffix].trim())
}

fn random_x_player() -> ProviderKind {
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u8(1);

    if hasher.finish() & 1 == 0 {
        ProviderKind::OpenAI
    } else {
        ProviderKind::Anthropic
    }
}

fn format_http_error(provider: &str, status: u16, body: &Value) -> String {
    if let Some(message) = body
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
    {
        return format!("{provider} returned HTTP {status}: {message}");
    }

    if let Some(message) = body.get("message").and_then(Value::as_str) {
        return format!("{provider} returned HTTP {status}: {message}");
    }

    format!("{provider} returned HTTP {status}: {body}")
}

fn usage() -> String {
    format!(
        "Usage:\n  OPENAI_API_KEY=... ANTHROPIC_API_KEY=... cargo run --example multi_llm_agents\n\nOptional environment variables:\n  OPENAI_MODEL (default: {DEFAULT_OPENAI_MODEL})\n  ANTHROPIC_MODEL (default: {DEFAULT_ANTHROPIC_MODEL})"
    )
}

fn run() -> Result<(), String> {
    let config = ExampleConfig::from_env()?;

    println!(
        "Starting tic-tac-toe match with OpenAI model `{}` and Anthropic model `{}`.\n",
        config.openai.model, config.anthropic.model
    );

    let (mut runtime, app) = boot_local_application(
        MultiLlmApplication {
            config: config.clone(),
        },
        Default::default(),
    )
    .map_err(|error| format!("boot application failed: {error:?}"))?;
    runtime.run_until_idle();

    let coordinator = runtime
        .resolve_name(COORDINATOR_NAME)
        .ok_or_else(|| "coordinator did not register".to_owned())?;
    wait_until_dead(&mut runtime, coordinator);

    let coordinator_exit = runtime
        .actor_snapshot(coordinator)
        .and_then(|snapshot| snapshot.metrics.last_exit)
        .ok_or_else(|| "coordinator exit reason was not retained".to_owned())?;

    let _ = runtime.exit_actor(app.root_supervisor(), ExitReason::Shutdown);
    runtime.run_until_idle();

    match coordinator_exit {
        ExitReason::Normal => Ok(()),
        other => Err(format!("coordinator exited abnormally: {other}")),
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}\n\n{}", usage());
        std::process::exit(1);
    }
}
