use crate::types::{ActorId, ExitReason};

pub(crate) fn panic_reason(
    actor: ActorId,
    name: &'static str,
    payload: Box<dyn std::any::Any + Send>,
) -> ExitReason {
    let detail = payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| {
            payload
                .downcast_ref::<&'static str>()
                .map(|message| (*message).to_owned())
        })
        .unwrap_or_else(|| "unknown panic".to_owned());

    ExitReason::Error(format!("actor `{name}` ({actor}) panicked: {detail}"))
}

pub(crate) fn mailbox_overflow_reason(actor: ActorId, label: &str) -> ExitReason {
    ExitReason::Error(format!(
        "actor `{actor}` mailbox overflow while delivering {label}"
    ))
}
