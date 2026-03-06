#![forbid(unsafe_code)]

//! `lamport` is an OTP-inspired runtime scaffold for Rust.
//!
//! The crate intentionally models the operational pieces that make Erlang/OTP
//! useful for services:
//!
//! - isolated actors with explicit ids and mailboxes
//! - async message delivery plus selective receive
//! - links, monitors, exit propagation, and trap-exit semantics
//! - supervisor flags, child specs, and restart-intensity windows
//! - scheduler-facing contracts for one-message turns and blocking pools
//!
//! Actor execution model:
//!
//! - the low-level [`Actor`] trait remains synchronous and turn-based
//! - async work should be modeled as message-driven state plus timers,
//!   request/reply, or blocking pool completions
//! - higher-level behaviours such as [`GenServer`], [`GenStatem`], and
//!   [`Supervisor`] build on top of that turn contract instead of replacing it
//!
//! Message-size policy:
//!
//! - mailboxes are bounded by envelope count, not byte size
//! - small inline payloads are fine for ordinary control flow
//! - large immutable payloads should be wrapped in shared buffers such as
//!   `Arc<[u8]>`, `Bytes`, or `Arc<str>`
//! - [`RECOMMENDED_INLINE_MESSAGE_LIMIT_BYTES`] is the soft threshold where
//!   shared immutable buffers become the preferred representation
//!
//! This crate is not BEAM-compatible. It is a compileable API baseline that
//! can evolve into a single-node OTP-style runtime.

pub mod actor;
pub mod application;
pub mod behaviour;
pub mod concurrent;
pub mod context;
pub mod envelope;
pub mod mailbox;
pub mod observability;
pub mod registry;
pub mod runtime;
pub mod scheduler;
pub mod supervisor;
pub mod types;

pub use actor::{Actor, ActorTurn};
pub use application::{
    Application, ApplicationHandle, boot_concurrent_application, boot_local_application,
};
pub use behaviour::{
    CallOutcome, CastMessage, GenServer, GenServerActor, GenStatem, GenStatemActor, InfoMessage,
    RuntimeInfo, ServerOutcome, StatemCallOutcome, StatemOutcome,
};
pub use concurrent::ConcurrentRuntime;
pub use context::{
    Context, LinkError, MonitorError, PendingCall, SendError, SpawnError, SpawnOptions, TaskHandle,
    TimerError,
};
pub use envelope::{
    CallTimedOut, DownMessage, Envelope, EnvelopeKind, ExitSignal, Message, Payload,
    RECOMMENDED_INLINE_MESSAGE_LIMIT_BYTES, ReplyToken, SystemMessage, TaskCompleted, TimerFired,
};
pub use mailbox::{Mailbox, MailboxWatermark};
pub use observability::{
    ActorIdentity, ActorTree, ActorTreeNode, EventCursor, RuntimeEvent, RuntimeEventKind,
    RuntimeIntrospection, RuntimeMetricsSnapshot,
};
pub use registry::{Registry, RegistryError};
pub use runtime::{ActorSnapshot, LocalRuntime, SupervisorChildSnapshot, SupervisorSnapshot};
pub use scheduler::{
    PoolKind, RunQueueSnapshot, ScheduleError, Scheduler, SchedulerConfig, SchedulerMetrics,
};
pub use supervisor::{
    RestartIntensity, StartChildError, Supervisor, SupervisorActor, SupervisorDirective,
    restart_scope,
};
pub use types::{
    ActorId, ActorMetrics, ActorStatus, ChildSpec, CrashReport, ExitReason, LifecycleEvent, Ref,
    Restart, Shutdown, ShutdownPhase, Strategy, SupervisorFlags, TimerToken,
};
