#![allow(dead_code)]

use std::{
    any::type_name,
    fmt::Debug,
    time::{Duration, Instant},
};

use lamport::{
    ActorId, ActorStatus, ConcurrentRuntime, LocalRuntime, Payload, SpawnError, StartChildError,
};

pub fn map_spawn_error(error: SpawnError) -> StartChildError {
    match error {
        SpawnError::InitFailed(reason) => StartChildError::InitFailed(reason),
        SpawnError::CapacityExceeded | SpawnError::Registry(_) => StartChildError::SpawnRejected,
    }
}

pub fn expect_downcast<T: Send + 'static>(payload: Payload, label: &str) -> T {
    payload.downcast::<T>().unwrap_or_else(|payload| {
        panic!(
            "expected {label} payload `{}`, got `{}`",
            type_name::<T>(),
            payload.type_name()
        )
    })
}

pub fn expect_some<T: Debug>(value: Option<T>, label: &str) -> T {
    value.unwrap_or_else(|| panic!("expected {label} to exist"))
}

pub fn wait_until_local<F>(
    runtime: &mut LocalRuntime,
    timeout: Duration,
    label: &str,
    mut predicate: F,
) where
    F: FnMut(&mut LocalRuntime) -> bool,
{
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        if predicate(runtime) {
            return;
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        let wait = remaining.min(Duration::from_millis(50));
        if wait.is_zero() {
            break;
        }

        let _ = runtime.block_on_next(Some(wait));
        runtime.run_until_idle();
    }

    assert!(
        predicate(runtime),
        "{label} did not complete within {:?}",
        timeout
    );
}

pub fn wait_until_local_dead(runtime: &mut LocalRuntime, actor: ActorId, timeout: Duration) {
    wait_until_local(
        runtime,
        timeout,
        &format!("actor {actor:?} to terminate"),
        |runtime| {
            runtime
                .actor_snapshot(actor)
                .is_some_and(|snapshot| snapshot.status == ActorStatus::Dead)
        },
    );
}

pub fn wait_until_concurrent<F>(
    runtime: &ConcurrentRuntime,
    timeout: Duration,
    label: &str,
    mut predicate: F,
) where
    F: FnMut(&ConcurrentRuntime) -> bool,
{
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        if predicate(runtime) {
            return;
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        let wait = remaining.min(Duration::from_millis(50));
        if wait.is_zero() {
            break;
        }

        let _ = runtime.wait_for_idle(Some(wait));
    }

    assert!(
        predicate(runtime),
        "{label} did not complete within {:?}",
        timeout
    );
}
