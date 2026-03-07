use crate::{envelope::ExitSignal, types::ExitReason};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExitDisposition {
    KillNow,
    Ignore,
    Enqueue,
}

pub(crate) fn classify_exit_signal(trap_exit: bool, signal: &ExitSignal) -> ExitDisposition {
    if matches!(signal.reason, ExitReason::Kill)
        || (signal.linked && !trap_exit && !matches!(signal.reason, ExitReason::Normal))
    {
        ExitDisposition::KillNow
    } else if signal.linked && !trap_exit && matches!(signal.reason, ExitReason::Normal) {
        ExitDisposition::Ignore
    } else {
        ExitDisposition::Enqueue
    }
}
