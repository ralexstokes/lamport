use lamport::{SpawnError, StartChildError};

pub fn map_spawn_error(error: SpawnError) -> StartChildError {
    match error {
        SpawnError::InitFailed(reason) => StartChildError::InitFailed(reason),
        SpawnError::CapacityExceeded | SpawnError::Registry(_) => StartChildError::SpawnRejected,
    }
}
