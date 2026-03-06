use std::collections::HashMap;

use crate::types::ActorId;

/// Failure while mutating the runtime name registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    /// The target actor does not exist in the runtime.
    NoProc(ActorId),
    /// Another actor already owns the requested name.
    NameTaken {
        /// Requested registry name.
        name: String,
        /// Actor that currently owns the name.
        actor: ActorId,
    },
    /// The actor already owns a different registered name.
    AlreadyRegistered {
        /// Actor attempting to register again.
        actor: ActorId,
        /// Existing registered name for the actor.
        name: String,
    },
}

/// Single-name-per-actor registry used for runtime lookup and registration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Registry {
    by_name: HashMap<String, ActorId>,
    by_actor: HashMap<ActorId, String>,
}

impl Registry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolves an actor by registered name.
    pub fn resolve(&self, name: &str) -> Option<ActorId> {
        self.by_name.get(name).copied()
    }

    /// Registers a name for the actor.
    pub fn register(&mut self, actor: ActorId, name: String) -> Result<(), RegistryError> {
        if let Some(existing_name) = self.by_actor.get(&actor) {
            if existing_name == &name {
                return Ok(());
            }

            return Err(RegistryError::AlreadyRegistered {
                actor,
                name: existing_name.clone(),
            });
        }

        if let Some(existing_actor) = self.by_name.get(&name).copied() {
            if existing_actor == actor {
                return Ok(());
            }

            return Err(RegistryError::NameTaken {
                name,
                actor: existing_actor,
            });
        }

        self.by_name.insert(name.clone(), actor);
        self.by_actor.insert(actor, name);
        Ok(())
    }

    /// Unregisters the current name for the actor and returns it.
    pub fn unregister(&mut self, actor: ActorId) -> Option<String> {
        let name = self.by_actor.remove(&actor)?;
        self.by_name.remove(&name);
        Some(name)
    }
}

#[cfg(test)]
mod tests {
    use crate::ActorId;

    use super::{Registry, RegistryError};

    #[test]
    fn registry_rejects_duplicate_names() {
        let actor_a = ActorId::new(1, 0);
        let actor_b = ActorId::new(2, 0);
        let mut registry = Registry::new();

        registry.register(actor_a, "worker".into()).unwrap();
        let error = registry.register(actor_b, "worker".into()).unwrap_err();

        assert_eq!(
            error,
            RegistryError::NameTaken {
                name: "worker".into(),
                actor: actor_a,
            }
        );
    }

    #[test]
    fn registry_unregisters_actor_names() {
        let actor = ActorId::new(3, 0);
        let mut registry = Registry::new();

        registry.register(actor, "cache".into()).unwrap();
        assert_eq!(registry.resolve("cache"), Some(actor));
        assert_eq!(registry.unregister(actor).as_deref(), Some("cache"));
        assert_eq!(registry.resolve("cache"), None);
    }
}
