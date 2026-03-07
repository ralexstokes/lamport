use crate::{
    concurrent::ConcurrentRuntime,
    context::{SpawnError, SpawnOptions},
    runtime::LocalRuntime,
    scheduler::SchedulerConfig,
    supervisor::Supervisor,
    types::ActorId,
};

/// Top-level boot contract for an OTP-style application.
pub trait Application: Send + 'static {
    /// Root supervisor behaviour that owns the application tree.
    type RootSupervisor: Supervisor;

    /// Human-readable application name used in handles and logs.
    fn name(&self) -> &'static str {
        std::any::type_name::<Self>()
    }

    /// Returns spawn options applied to the root supervisor.
    fn root_options(&self) -> SpawnOptions {
        SpawnOptions::default()
    }

    /// Builds the root supervisor for the application tree.
    fn root_supervisor(self) -> Self::RootSupervisor;
}

/// Handle returned after an application root supervisor is booted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApplicationHandle {
    name: &'static str,
    root_supervisor: ActorId,
}

impl ApplicationHandle {
    /// Creates a new application handle.
    pub const fn new(name: &'static str, root_supervisor: ActorId) -> Self {
        Self {
            name,
            root_supervisor,
        }
    }

    /// Returns the application name.
    pub const fn name(self) -> &'static str {
        self.name
    }

    /// Returns the booted root supervisor actor id.
    pub const fn root_supervisor(self) -> ActorId {
        self.root_supervisor
    }
}

/// Creates a local runtime, boots an application root supervisor, and returns both.
pub fn boot_local_application<A: Application>(
    application: A,
    config: SchedulerConfig,
) -> Result<(LocalRuntime, ApplicationHandle), SpawnError> {
    let mut runtime = LocalRuntime::new(config);
    let handle = runtime.boot_application(application)?;
    Ok((runtime, handle))
}

/// Creates a concurrent runtime, boots an application root supervisor, and returns both.
pub fn boot_concurrent_application<A: Application>(
    application: A,
    config: SchedulerConfig,
) -> Result<(ConcurrentRuntime, ApplicationHandle), SpawnError> {
    let runtime = ConcurrentRuntime::new(config);
    let handle = runtime.boot_application(application)?;
    Ok((runtime, handle))
}

impl LocalRuntime {
    /// Boots an application root supervisor into an existing local runtime.
    pub fn boot_application<A: Application>(
        &mut self,
        application: A,
    ) -> Result<ApplicationHandle, SpawnError> {
        let (name, options, root_supervisor) = prepare_application(application);
        let root = self.spawn_supervisor_with_options(root_supervisor, options)?;
        Ok(ApplicationHandle::new(name, root))
    }
}

impl ConcurrentRuntime {
    /// Boots an application root supervisor into an existing concurrent runtime.
    pub fn boot_application<A: Application>(
        &self,
        application: A,
    ) -> Result<ApplicationHandle, SpawnError> {
        let (name, options, root_supervisor) = prepare_application(application);
        let root = self.spawn_supervisor_with_options(root_supervisor, options)?;
        Ok(ApplicationHandle::new(name, root))
    }
}

fn prepare_application<A: Application>(
    application: A,
) -> (&'static str, SpawnOptions, A::RootSupervisor) {
    let name = application.name();
    let options = application.root_options();
    let root_supervisor = application.root_supervisor();
    (name, options, root_supervisor)
}

#[cfg(test)]
mod tests {
    use crate::{
        Context, ExitReason, LocalRuntime, SpawnOptions, StartChildError, Supervisor,
        SupervisorDirective, SupervisorFlags,
        application::{Application, boot_local_application},
        types::{ChildSpec, Strategy},
    };

    struct EmptyRootSupervisor {
        flags: SupervisorFlags,
        specs: Vec<ChildSpec>,
    }

    impl Supervisor for EmptyRootSupervisor {
        fn flags(&self) -> SupervisorFlags {
            self.flags
        }

        fn child_specs(&self) -> &[ChildSpec] {
            &self.specs
        }

        fn start_child<C: Context>(
            &mut self,
            _spec: &ChildSpec,
            _ctx: &mut C,
        ) -> Result<crate::ActorId, StartChildError> {
            Err(StartChildError::InitFailed(ExitReason::Error(
                "empty application has no children".into(),
            )))
        }

        fn on_child_exit<C: Context>(
            &mut self,
            _spec: &ChildSpec,
            _actor: crate::ActorId,
            _reason: ExitReason,
            _ctx: &mut C,
        ) -> SupervisorDirective {
            SupervisorDirective::Ignore
        }
    }

    struct EmptyApplication;

    impl Application for EmptyApplication {
        type RootSupervisor = EmptyRootSupervisor;

        fn name(&self) -> &'static str {
            "empty"
        }

        fn root_options(&self) -> SpawnOptions {
            SpawnOptions {
                registered_name: Some("empty.root".into()),
                ..SpawnOptions::default()
            }
        }

        fn root_supervisor(self) -> Self::RootSupervisor {
            EmptyRootSupervisor {
                flags: SupervisorFlags {
                    strategy: Strategy::OneForOne,
                    ..SupervisorFlags::default()
                },
                specs: Vec::new(),
            }
        }
    }

    #[test]
    fn boot_local_application_spawns_root_supervisor() {
        let (mut runtime, handle) =
            boot_local_application(EmptyApplication, Default::default()).unwrap();

        runtime.run_until_idle();

        assert_eq!(handle.name(), "empty");
        assert_eq!(
            runtime.resolve_name("empty.root"),
            Some(handle.root_supervisor())
        );

        let snapshot = runtime
            .supervisor_snapshot(handle.root_supervisor())
            .expect("root supervisor should expose a snapshot");
        assert_eq!(snapshot.children.len(), 0);
        assert_eq!(snapshot.flags.strategy, Strategy::OneForOne);
    }

    #[test]
    fn existing_runtime_can_boot_application() {
        let mut runtime = LocalRuntime::default();
        let handle = runtime.boot_application(EmptyApplication).unwrap();

        runtime.run_until_idle();

        assert!(runtime.actor_snapshot(handle.root_supervisor()).is_some());
    }
}
