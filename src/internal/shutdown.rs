use tokio::task::JoinHandle;

use crate::types::{ActorId, Shutdown};

#[derive(Debug)]
pub(crate) struct ShutdownTracker {
    pub(crate) requester: ActorId,
    pub(crate) policy: Shutdown,
    pub(crate) task: Option<JoinHandle<()>>,
}
