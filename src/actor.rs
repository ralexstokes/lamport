use std::any::type_name;

use crate::{
    context::Context,
    control::{ControlError, StateSnapshot},
    envelope::Envelope,
    types::ExitReason,
};

/// Outcome of a single actor turn on a scheduler thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActorTurn {
    /// Continue processing future messages without forcing an immediate yield.
    Continue,
    /// Yield the scheduler after finishing the current envelope.
    Yield,
    /// Stop the actor with the given exit reason.
    Stop(ExitReason),
}

/// The low-level runtime contract for a single actor.
///
/// The trait intentionally stays synchronous and turn-based so the runtime can
/// preserve "one envelope, one turn" scheduling invariants. Async workflows are
/// expected to be modeled with higher-level behaviours, timers, request/reply,
/// and blocking task completions routed back through the mailbox.
pub trait Actor: Send + 'static {
    /// Returns a human-readable actor name for tracing and crash reports.
    fn name(&self) -> &'static str {
        type_name::<Self>()
    }

    /// Runs once when the actor starts.
    fn init<C: Context>(&mut self, _ctx: &mut C) -> Result<(), ExitReason> {
        Ok(())
    }

    /// Handles exactly one delivered envelope.
    fn handle<C: Context>(&mut self, envelope: Envelope, ctx: &mut C) -> ActorTurn;

    /// Returns the actor's current control-plane state version.
    fn state_version(&self) -> u64 {
        0
    }

    /// Returns a type-erased state snapshot for runtime inspection.
    fn inspect_state<C: Context>(&mut self, _ctx: &mut C) -> Result<StateSnapshot, ControlError> {
        Err(ControlError::unsupported("GetState"))
    }

    /// Replaces the actor's internal state from a validated control payload.
    fn replace_state<C: Context>(
        &mut self,
        _snapshot: StateSnapshot,
        _ctx: &mut C,
    ) -> Result<(), ControlError> {
        Err(ControlError::unsupported("ReplaceState"))
    }

    /// Runs a reserved code-change hook through the runtime control path.
    fn code_change<C: Context>(
        &mut self,
        target_version: u64,
        _ctx: &mut C,
    ) -> Result<(), ControlError> {
        let current = self.state_version();
        if current == target_version {
            Ok(())
        } else {
            Err(ControlError::VersionMismatch {
                current,
                requested: target_version,
            })
        }
    }

    /// Runs once when the actor is exiting.
    fn terminate<C: Context>(&mut self, _reason: ExitReason, _ctx: &mut C) {}
}
