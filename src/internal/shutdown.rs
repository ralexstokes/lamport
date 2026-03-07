use std::time::Duration;

use tokio::task::JoinHandle;

use crate::{
    envelope::ExitSignal,
    types::{ActorId, ExitReason, Shutdown},
};

#[derive(Debug)]
pub(crate) struct ShutdownTracker {
    pub(crate) requester: ActorId,
    pub(crate) policy: Shutdown,
    pub(crate) task: Option<JoinHandle<()>>,
}

impl ShutdownTracker {
    pub(crate) const fn new(
        requester: ActorId,
        policy: Shutdown,
        task: Option<JoinHandle<()>>,
    ) -> Self {
        Self {
            requester,
            policy,
            task,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShutdownMode {
    ForceKill,
    Linked { timeout: Option<Duration> },
}

impl ShutdownMode {
    pub(crate) const fn from_policy(policy: &Shutdown) -> Self {
        match policy {
            Shutdown::BrutalKill => Self::ForceKill,
            Shutdown::Timeout(delay) => Self::Linked {
                timeout: Some(*delay),
            },
            Shutdown::Infinity => Self::Linked { timeout: None },
        }
    }
}

pub(crate) const fn shutdown_signal(requester: ActorId) -> ExitSignal {
    ExitSignal {
        from: requester,
        reason: ExitReason::Shutdown,
        linked: true,
    }
}

pub(crate) fn shutdown_link_reason(
    shutdown_requested: bool,
    final_reason: &ExitReason,
) -> ExitReason {
    if shutdown_requested && matches!(final_reason, ExitReason::Kill) {
        ExitReason::Shutdown
    } else {
        final_reason.clone()
    }
}
