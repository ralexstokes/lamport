use crate::{
    actor::ActorTurn,
    envelope::{Envelope, Message, Payload, ReplyToken},
    types::ExitReason,
};

use super::{CastMessage, InfoMessage, RuntimeInfo};

pub(super) enum DispatchEnvelope {
    Request { token: ReplyToken, message: Payload },
    User(Payload),
    Runtime(RuntimeInfo),
}

pub(super) enum UserMessage<C, I> {
    Cast(C),
    Info(I),
}

pub(super) fn classify_envelope(envelope: Envelope) -> DispatchEnvelope {
    match envelope {
        Envelope::Request { token, message } => DispatchEnvelope::Request { token, message },
        Envelope::User(payload) => DispatchEnvelope::User(payload),
        Envelope::Reply { reference, message } => {
            DispatchEnvelope::Runtime(RuntimeInfo::Reply { reference, message })
        }
        Envelope::Task(task) => DispatchEnvelope::Runtime(RuntimeInfo::Task(task)),
        Envelope::CallTimeout(timeout) => {
            DispatchEnvelope::Runtime(RuntimeInfo::CallTimeout(timeout))
        }
        Envelope::Exit(signal) => DispatchEnvelope::Runtime(RuntimeInfo::Exit(signal)),
        Envelope::Down(message) => DispatchEnvelope::Runtime(RuntimeInfo::Down(message)),
        Envelope::Timer(timer) => DispatchEnvelope::Runtime(RuntimeInfo::Timer(timer)),
        Envelope::System(message) => DispatchEnvelope::Runtime(RuntimeInfo::System(message)),
    }
}

pub(super) fn downcast_payload<M: Message>(payload: Payload, label: &str) -> Result<M, ActorTurn> {
    match payload.downcast::<M>() {
        Ok(message) => Ok(message),
        Err(payload) => Err(ActorTurn::Stop(ExitReason::Error(format!(
            "unexpected {label} payload `{}`",
            payload.type_name()
        )))),
    }
}

pub(super) fn downcast_user_message<C: Message, I: Message>(
    payload: Payload,
    actor: &str,
) -> Result<UserMessage<C, I>, ActorTurn> {
    match payload.downcast::<CastMessage<C>>() {
        Ok(CastMessage(message)) => Ok(UserMessage::Cast(message)),
        Err(payload) => match payload.downcast::<InfoMessage<I>>() {
            Ok(InfoMessage(message)) => Ok(UserMessage::Info(message)),
            Err(payload) => Err(ActorTurn::Stop(ExitReason::Error(format!(
                "unexpected {actor} user payload `{}`",
                payload.type_name()
            )))),
        },
    }
}
