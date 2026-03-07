use std::{collections::BTreeMap, time::Instant};

use crate::{
    snapshot::{SupervisorChildSnapshot, SupervisorSnapshot},
    supervisor::RestartIntensity,
    types::{ActorId, ChildSpec, SupervisorFlags},
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

    pub(crate) fn reconfigure(&mut self, flags: SupervisorFlags, child_specs: Vec<ChildSpec>) {
        self.flags = flags;
        self.child_specs = child_specs;
        self.running
            .retain(|child_id, _| self.child_specs.iter().any(|spec| spec.id == *child_id));
        self.intensity.reconfigure(flags);
    }
}
