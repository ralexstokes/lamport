use crate::types::ExitReason;

#[derive(Debug, Default)]
pub(crate) struct TurnEffects {
    pub(crate) exit_reason: Option<ExitReason>,
    pub(crate) yielded: bool,
}
