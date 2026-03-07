use crate::types::{ActorId, ActorMetrics, ActorStatus, ChildSpec, Ref, SupervisorFlags};

/// Snapshot of actor state exposed for runtime introspection and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActorSnapshot {
    /// Runtime id for the actor instance.
    pub id: ActorId,
    /// Human-readable actor name used in traces and crash reports.
    pub name: &'static str,
    /// Optional registered name.
    pub registered_name: Option<String>,
    /// Parent actor in the supervision tree.
    pub parent: Option<ActorId>,
    /// Ancestor chain captured at spawn time.
    pub ancestors: Vec<ActorId>,
    /// Link set for the actor.
    pub links: Vec<ActorId>,
    /// Actors monitoring this actor by reference.
    pub monitors_in: Vec<(Ref, ActorId)>,
    /// Actors monitored by this actor by reference.
    pub monitors_out: Vec<(Ref, ActorId)>,
    /// Whether linked exits are trapped into the mailbox.
    pub trap_exit: bool,
    /// Current lifecycle status.
    pub status: ActorStatus,
    /// Runtime metrics for the actor.
    pub metrics: ActorMetrics,
}

/// Child state tracked by the runtime for a supervisor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorChildSnapshot {
    /// Static child spec.
    pub spec: ChildSpec,
    /// Running actor id for the child, if any.
    pub actor: Option<ActorId>,
}

/// Snapshot of live supervisor metadata tracked by the runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorSnapshot {
    /// Supervisor actor id.
    pub actor: ActorId,
    /// Supervisor restart flags.
    pub flags: SupervisorFlags,
    /// Ordered child specs with current running ids.
    pub children: Vec<SupervisorChildSnapshot>,
    /// Restarts currently inside the active intensity window.
    pub active_restarts: usize,
}
