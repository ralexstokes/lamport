use std::time::{Duration, Instant};

use lamport::{ActorId, ActorStatus, ConcurrentRuntime, LocalRuntime};

#[allow(dead_code)]
pub fn wait_until_local_actor_dead(
    runtime: &mut LocalRuntime,
    actor: ActorId,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        if runtime
            .actor_snapshot(actor)
            .is_some_and(|snapshot| snapshot.status == ActorStatus::Dead)
        {
            return Ok(());
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        let wait = remaining.min(Duration::from_millis(50));
        let _ = runtime.block_on_next(Some(wait));
        runtime.run_until_idle();
    }

    Err(format!(
        "actor {actor} did not terminate within {timeout:?}"
    ))
}

#[allow(dead_code)]
pub fn wait_until_concurrent_actor_dead(
    runtime: &ConcurrentRuntime,
    actor: ActorId,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        if runtime
            .actor_snapshot(actor)
            .is_some_and(|snapshot| snapshot.status == ActorStatus::Dead)
        {
            return Ok(());
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        let wait = remaining.min(Duration::from_millis(250));
        let _ = runtime.wait_for_idle(Some(wait));
    }

    Err(format!(
        "actor {actor} did not terminate within {timeout:?}"
    ))
}
