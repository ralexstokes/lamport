use std::fmt;

use crate::types::{ActorId, ActorIdentity, ExitReason, Ref, Shutdown};

/// Lifecycle phases for runtime-managed actor shutdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownPhase {
    /// A graceful or forced shutdown was requested.
    Requested,
    /// A graceful shutdown exceeded its timeout and was escalated.
    TimedOut,
    /// The actor finished exiting after a shutdown request.
    Completed,
}

/// Structured lifecycle events emitted by the runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleEvent {
    /// A new actor was spawned into the runtime.
    Spawn {
        /// The spawned actor id.
        actor: ActorId,
        /// Human-readable actor name.
        name: &'static str,
        /// Optional registered name for the actor.
        registered_name: Option<String>,
        /// Parent actor in the supervision tree, if any.
        parent: Option<ActorId>,
        /// Supervisor child spec id when the actor belongs to a supervisor.
        supervisor_child: Option<&'static str>,
    },
    /// An actor exited and completed termination.
    Exit {
        /// The terminated actor id.
        actor: ActorId,
        /// Human-readable actor name.
        name: &'static str,
        /// Final exit reason.
        reason: ExitReason,
        /// Parent actor in the supervision tree, if any.
        parent: Option<ActorId>,
        /// Ancestor chain captured at spawn time.
        ancestors: Vec<ActorId>,
    },
    /// A monitor delivered a `DOWN` notification.
    Down {
        /// Watching actor that received the notification.
        watcher: ActorId,
        /// Terminated actor.
        actor: ActorId,
        /// Monitor reference.
        reference: Ref,
        /// Exit reason observed by the watcher.
        reason: ExitReason,
    },
    /// A supervisor restarted a child.
    Restart {
        /// Supervisor actor performing the restart.
        supervisor: ActorId,
        /// Stable child id from the supervisor spec.
        child_id: &'static str,
        /// Previous actor id for the child, if one existed.
        old_actor: Option<ActorId>,
        /// Newly spawned actor id.
        new_actor: ActorId,
    },
    /// A shutdown request was issued, timed out, or completed.
    Shutdown {
        /// Actor that initiated the shutdown.
        requester: ActorId,
        /// Actor being shut down.
        actor: ActorId,
        /// Shutdown policy in effect.
        policy: Shutdown,
        /// Current phase of the shutdown flow.
        phase: ShutdownPhase,
        /// Final or escalated reason when available.
        reason: Option<ExitReason>,
    },
}

/// Structured crash report emitted for abnormal actor exits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashReport {
    /// Actor that crashed or was killed.
    pub actor: ActorId,
    /// Human-readable actor name.
    pub name: &'static str,
    /// Optional registered actor name.
    pub registered_name: Option<String>,
    /// Parent actor in the supervision tree, if any.
    pub parent: Option<ActorId>,
    /// Resolved parent actor identity when it was still available.
    pub parent_context: Option<ActorIdentity>,
    /// Ancestor chain captured at spawn time.
    pub ancestors: Vec<ActorId>,
    /// Resolved ancestor identities from root to leaf when available.
    pub ancestor_contexts: Vec<ActorIdentity>,
    /// Supervisor child spec id when the actor belongs to a supervisor.
    pub supervisor_child: Option<&'static str>,
    /// Final abnormal exit reason.
    pub reason: ExitReason,
}

impl CrashReport {
    /// Returns the crashing actor as a reusable identity object.
    pub fn actor_identity(&self) -> ActorIdentity {
        ActorIdentity::new(self.actor, self.name, self.registered_name.clone())
    }

    /// Returns the resolved parent actor identity when available.
    pub fn parent_identity(&self) -> Option<&ActorIdentity> {
        self.parent_context.as_ref()
    }

    /// Returns resolved ancestor identities in root-to-leaf order.
    pub fn ancestor_identities(&self) -> &[ActorIdentity] {
        &self.ancestor_contexts
    }
}

impl fmt::Display for CrashReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} exited abnormally: {}",
            self.actor_identity(),
            self.reason
        )?;

        if let Some(child_id) = self.supervisor_child {
            write!(f, " [supervisor child: {child_id}]")?;
        }

        if let Some(parent) = self.parent_context.as_ref() {
            write!(f, "; parent: {parent}")?;
        } else if let Some(parent) = self.parent {
            write!(f, "; parent: {parent}")?;
        }

        if !self.ancestor_contexts.is_empty() {
            write_ancestor_chain(f, &self.ancestor_contexts)?;
        } else if !self.ancestors.is_empty() {
            write_ancestor_chain(f, &self.ancestors)?;
        }

        Ok(())
    }
}

fn write_ancestor_chain(f: &mut fmt::Formatter<'_>, items: &[impl fmt::Display]) -> fmt::Result {
    f.write_str("; ancestors: ")?;
    for (index, ancestor) in items.iter().enumerate() {
        if index > 0 {
            f.write_str(" -> ")?;
        }
        write!(f, "{ancestor}")?;
    }
    Ok(())
}
