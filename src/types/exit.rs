use std::fmt;

use super::ActorId;

/// Exit reasons modeled after the OTP failure protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitReason {
    /// Clean actor completion.
    Normal,
    /// Ordered shutdown from a supervisor or caller.
    Shutdown,
    /// Untrappable kill signal.
    Kill,
    /// Target actor does not exist.
    NoProc,
    /// Target node is unavailable.
    NoConnection,
    /// Arbitrary runtime or application error.
    Error(String),
}

impl fmt::Display for ExitReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Normal => f.write_str("normal"),
            Self::Shutdown => f.write_str("shutdown"),
            Self::Kill => f.write_str("killed"),
            Self::NoProc => f.write_str("no process"),
            Self::NoConnection => f.write_str("no connection"),
            Self::Error(detail) => write!(f, "error: {detail}"),
        }
    }
}

impl std::error::Error for ExitReason {}

/// Identifies an actor in crash reports and topology snapshots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActorIdentity {
    /// Stable runtime id for the actor.
    pub actor: ActorId,
    /// Human-readable actor name.
    pub name: &'static str,
    /// Optional registered name.
    pub registered_name: Option<String>,
}

impl ActorIdentity {
    /// Creates an actor identity from runtime metadata.
    pub fn new(actor: ActorId, name: &'static str, registered_name: Option<String>) -> Self {
        Self {
            actor,
            name,
            registered_name,
        }
    }
}

impl fmt::Display for ActorIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} `{}`", self.actor, self.name)?;
        if let Some(name) = self.registered_name.as_deref() {
            write!(f, " ({name})")?;
        }
        Ok(())
    }
}
