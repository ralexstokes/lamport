mod actor;
mod exit;
mod status;
mod supervision;

pub use actor::{ActorId, Ref, TimerToken};
pub use exit::{ActorIdentity, ExitReason};
pub use status::{ActorMetrics, ActorStatus};
pub use supervision::{ChildSpec, Restart, Shutdown, Strategy, SupervisorFlags};

#[cfg(test)]
mod tests {
    use super::{ChildSpec, ExitReason, Restart, Shutdown};

    #[test]
    fn transient_children_restart_only_on_abnormal_exit() {
        let spec = ChildSpec {
            id: "worker",
            restart: Restart::Transient,
            shutdown: Shutdown::default(),
            is_supervisor: false,
        };

        assert!(!spec.should_restart(&ExitReason::Normal));
        assert!(!spec.should_restart(&ExitReason::Shutdown));
        assert!(spec.should_restart(&ExitReason::Kill));
        assert!(spec.should_restart(&ExitReason::Error("boom".into())));
    }
}
