use std::{collections::BTreeMap, time::Instant};

use tokio::task::JoinHandle;

use crate::{
    runtime::{SupervisorChildSnapshot, SupervisorSnapshot},
    supervisor::RestartIntensity,
    types::{ActorId, ChildSpec, ExitReason, Shutdown, SupervisorFlags},
};

#[derive(Debug, Clone)]
pub(crate) struct SupervisorRuntimeState {
    pub(crate) flags: SupervisorFlags,
    pub(crate) child_specs: Vec<ChildSpec>,
    pub(crate) running: BTreeMap<&'static str, ActorId>,
    pub(crate) intensity: RestartIntensity,
}

impl SupervisorRuntimeState {
    pub(crate) fn new(flags: SupervisorFlags, child_specs: Vec<ChildSpec>) -> Self {
        Self {
            flags,
            child_specs,
            running: BTreeMap::new(),
            intensity: RestartIntensity::new(flags),
        }
    }

    pub(crate) fn set_running(&mut self, child_id: &'static str, actor: ActorId) {
        self.running.insert(child_id, actor);
    }

    pub(crate) fn clear_running(&mut self, child_id: &'static str, actor: ActorId) {
        if self.running.get(child_id).copied() == Some(actor) {
            self.running.remove(child_id);
        }
    }

    pub(crate) fn snapshot(&mut self, actor: ActorId) -> SupervisorSnapshot {
        SupervisorSnapshot {
            actor,
            flags: self.flags,
            children: self
                .child_specs
                .iter()
                .cloned()
                .map(|spec| SupervisorChildSnapshot {
                    actor: self.running.get(spec.id).copied(),
                    spec,
                })
                .collect(),
            active_restarts: self.intensity.active_restarts(Instant::now()),
        }
    }
}

#[derive(Debug)]
pub(crate) struct ShutdownTracker {
    pub(crate) requester: ActorId,
    pub(crate) policy: Shutdown,
    pub(crate) task: Option<JoinHandle<()>>,
}

#[derive(Debug, Default)]
pub(crate) struct TurnEffects {
    pub(crate) exit_reason: Option<ExitReason>,
    pub(crate) yielded: bool,
}

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
        "actor `{actor:?}` mailbox overflow while delivering {label}"
    ))
}
