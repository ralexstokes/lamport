use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::types::{ActorId, ChildSpec, ExitReason, SupervisorFlags};

#[derive(Debug)]
pub(super) struct SupervisorActorState {
    pub(super) flags: SupervisorFlags,
    pub(super) specs: Vec<ChildSpec>,
    pub(super) running: BTreeMap<&'static str, ActorId>,
    pub(super) by_actor: BTreeMap<ActorId, &'static str>,
    pub(super) action: Option<SupervisorAction>,
}

#[derive(Debug)]
pub(super) enum SupervisorAction {
    Restart(RestartPlan),
    Shutdown(ShutdownPlan),
}

#[derive(Debug)]
pub(super) struct RestartPlan {
    pub(super) active_shutdown: Option<&'static str>,
    pub(super) shutdown_queue: VecDeque<&'static str>,
    pub(super) restart_queue: VecDeque<&'static str>,
    pub(super) old_children: BTreeMap<&'static str, Option<ActorId>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RestartPlanError {
    UnexpectedChild(&'static str),
}

#[derive(Debug)]
pub(super) struct ShutdownPlan {
    pub(super) active_child: Option<&'static str>,
    pub(super) shutdown_queue: VecDeque<&'static str>,
    pub(super) final_reason: ExitReason,
}

impl SupervisorActorState {
    pub(super) fn new(flags: SupervisorFlags, specs: Vec<ChildSpec>) -> Self {
        Self {
            flags,
            specs,
            running: BTreeMap::new(),
            by_actor: BTreeMap::new(),
            action: None,
        }
    }

    pub(super) fn spec(&self, child_id: &'static str) -> Option<&ChildSpec> {
        self.specs.iter().find(|spec| spec.id == child_id)
    }

    pub(super) fn running_actor(&self, child_id: &'static str) -> Option<ActorId> {
        self.running.get(child_id).copied()
    }

    pub(super) fn remember_child(&mut self, child_id: &'static str, actor: ActorId) {
        self.running.insert(child_id, actor);
        self.by_actor.insert(actor, child_id);
    }

    pub(super) fn forget_child_actor(&mut self, actor: ActorId) -> Option<&'static str> {
        let child_id = self.by_actor.remove(&actor)?;
        self.running.remove(child_id);
        Some(child_id)
    }

    pub(super) fn ordered_child_ids(&self, requested: &[&'static str]) -> Vec<&'static str> {
        let requested: BTreeSet<_> = requested.iter().copied().collect();
        self.specs
            .iter()
            .filter_map(|spec| requested.contains(&spec.id).then_some(spec.id))
            .collect()
    }

    pub(super) fn running_children_reverse(
        &self,
        child_ids: &[&'static str],
    ) -> VecDeque<&'static str> {
        let child_ids: BTreeSet<_> = child_ids.iter().copied().collect();
        self.specs
            .iter()
            .rev()
            .filter_map(|spec| {
                (child_ids.contains(&spec.id) && self.running.contains_key(spec.id))
                    .then_some(spec.id)
            })
            .collect()
    }

    pub(super) fn all_running_reverse(&self) -> VecDeque<&'static str> {
        self.specs
            .iter()
            .rev()
            .filter_map(|spec| self.running.contains_key(spec.id).then_some(spec.id))
            .collect()
    }
}

impl RestartPlan {
    pub(super) fn complete_shutdown(
        &mut self,
        child_id: &'static str,
    ) -> Result<(), RestartPlanError> {
        if self.active_shutdown == Some(child_id) {
            self.active_shutdown = None;
            return Ok(());
        }

        if let Some(index) = self
            .shutdown_queue
            .iter()
            .position(|queued| *queued == child_id)
        {
            self.shutdown_queue.remove(index);
            return Ok(());
        }

        Err(RestartPlanError::UnexpectedChild(child_id))
    }

    pub(super) fn next_shutdown(&mut self) -> Option<&'static str> {
        let next_child = self.shutdown_queue.pop_front();
        self.active_shutdown = next_child;
        next_child
    }
}

impl ShutdownPlan {
    pub(super) fn complete_child(&mut self, child_id: &'static str) {
        if self.active_child == Some(child_id) {
            self.active_child = None;
            return;
        }

        if let Some(index) = self
            .shutdown_queue
            .iter()
            .position(|queued| *queued == child_id)
        {
            self.shutdown_queue.remove(index);
        }
    }

    pub(super) fn next_child(&mut self) -> Option<&'static str> {
        let next_child = self.shutdown_queue.pop_front();
        self.active_child = next_child;
        next_child
    }
}
