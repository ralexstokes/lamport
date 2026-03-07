use std::{collections::BTreeMap, env};

use lamport::{
    CallOutcome, Context, ExitReason, GenServer, Ref, ReplyToken, ServerOutcome,
    behaviour::RuntimeInfo,
};

use super::{DEFAULT_ANTHROPIC_MODEL, DEFAULT_OPENAI_MODEL, http::request_provider};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum ProviderKind {
    OpenAI,
    Anthropic,
}

impl ProviderKind {
    pub(super) const ALL: [Self; 2] = [Self::OpenAI, Self::Anthropic];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::OpenAI => "openai",
            Self::Anthropic => "anthropic",
        }
    }

    pub(super) fn child_id(self) -> &'static str {
        match self {
            Self::OpenAI => "openai-provider",
            Self::Anthropic => "anthropic-provider",
        }
    }

    pub(super) fn registered_name(self) -> &'static str {
        match self {
            Self::OpenAI => "openai-agent",
            Self::Anthropic => "anthropic-agent",
        }
    }

    pub(super) fn opponent(self) -> Self {
        match self {
            Self::OpenAI => Self::Anthropic,
            Self::Anthropic => Self::OpenAI,
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct ProviderConfig {
    pub(super) kind: ProviderKind,
    pub(super) model: String,
    pub(super) api_key: String,
}

impl ProviderConfig {
    pub(super) fn prompt_cache_key(&self) -> String {
        format!("multi-llm-agents/{}/{}", self.kind.label(), self.model)
    }
}

#[derive(Debug, Clone)]
pub(super) struct ExampleConfig {
    pub(super) openai: ProviderConfig,
    pub(super) anthropic: ProviderConfig,
}

impl ExampleConfig {
    pub(super) fn from_env() -> Result<Self, String> {
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
pub(super) enum ProviderCall {
    ChooseMove {
        system_prompt: String,
        update: String,
    },
}

#[derive(Debug, Clone)]
pub(super) enum ProviderReply {
    Success(ProviderAnswer),
    Failure(ProviderFailure),
}

#[derive(Debug, Clone)]
pub(super) struct ProviderAnswer {
    pub(super) model: String,
    pub(super) text: String,
    pub(super) cache_summary: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct ProviderFailure {
    pub(super) model: String,
    pub(super) error: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ConversationRole {
    User,
    Assistant,
}

impl ConversationRole {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct ConversationMessage {
    pub(super) role: ConversationRole,
    pub(super) content: String,
}

impl ConversationMessage {
    fn user(content: String) -> Self {
        Self {
            role: ConversationRole::User,
            content,
        }
    }

    fn assistant(content: String) -> Self {
        Self {
            role: ConversationRole::Assistant,
            content,
        }
    }
}

#[derive(Debug, Clone)]
struct ProviderConversation {
    system_prompt: String,
    messages: Vec<ConversationMessage>,
}

impl ProviderConversation {
    fn new(system_prompt: String) -> Self {
        Self {
            system_prompt,
            messages: Vec::new(),
        }
    }

    fn build_request(&self, update: &str) -> ProviderRequest {
        let mut messages = self.messages.clone();
        messages.push(ConversationMessage::user(update.to_owned()));
        ProviderRequest {
            system_prompt: self.system_prompt.clone(),
            messages,
        }
    }

    fn record_success(&mut self, update: String, reply: String) {
        self.messages.push(ConversationMessage::user(update));
        self.messages.push(ConversationMessage::assistant(reply));
    }
}

#[derive(Debug, Clone)]
pub(super) struct ProviderRequest {
    pub(super) system_prompt: String,
    pub(super) messages: Vec<ConversationMessage>,
}

#[derive(Debug, Clone)]
struct PendingProviderCall {
    reply_to: ReplyToken,
    update: String,
}

#[derive(Debug, Default)]
pub(super) struct ProviderState {
    pending: BTreeMap<Ref, PendingProviderCall>,
    conversation: Option<ProviderConversation>,
}

impl ProviderState {
    fn conversation_mut(&mut self, system_prompt: String) -> &mut ProviderConversation {
        self.conversation
            .get_or_insert_with(|| ProviderConversation::new(system_prompt))
    }
}

pub(super) struct ProviderServer {
    config: ProviderConfig,
}

impl ProviderServer {
    pub(super) fn new(config: ProviderConfig) -> Self {
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
            ProviderCall::ChooseMove {
                system_prompt,
                update,
            } => {
                let config = self.config.clone();
                let request = state.conversation_mut(system_prompt).build_request(&update);
                let task = ctx.spawn_blocking_io(move || request_provider(&config, request));
                state.pending.insert(
                    task.id(),
                    PendingProviderCall {
                        reply_to: from,
                        update,
                    },
                );
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
                let Some(pending) = state.pending.remove(&task.reference) else {
                    return ServerOutcome::Continue;
                };

                let result = match task.result.downcast::<Result<ProviderAnswer, String>>() {
                    Ok(result) => result,
                    Err(payload) => {
                        let payload_type = payload.type_name();
                        return ServerOutcome::Stop(ExitReason::Error(format!(
                            "unexpected provider task result `{payload_type}`",
                        )));
                    }
                };

                let reply = match result {
                    Ok(answer) => {
                        let Some(conversation) = state.conversation.as_mut() else {
                            return ServerOutcome::Stop(ExitReason::Error(
                                "provider conversation missing while completing request".into(),
                            ));
                        };
                        conversation.record_success(pending.update, answer.text.clone());
                        ProviderReply::Success(answer)
                    }
                    Err(error) => ProviderReply::Failure(ProviderFailure {
                        model: self.config.model.clone(),
                        error,
                    }),
                };

                let _ = ctx.reply(pending.reply_to, reply);
                ServerOutcome::Continue
            }
            _ => ServerOutcome::Continue,
        }
    }
}
