use std::{marker::PhantomData, time::Duration};

use crate::{
    actor::Actor,
    behaviour::{GenServer, GenServerActor, GenStatem, GenStatemActor, RuntimeInfo},
    envelope::{Envelope, Message, ReplyToken},
    lifecycle::LifecycleEvent,
    mailbox::MailboxWatermark,
    registry::RegistryError,
    scheduler::PoolKind,
    supervisor::{Supervisor, SupervisorActor},
    types::{
        ActorId, ChildSpec, ExitReason, ProcessAddr, Ref, Shutdown, SupervisorFlags, TimerToken,
    },
};

/// Options applied when spawning a new actor.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SpawnOptions {
    /// Optional registered name for introspection or lookup tables.
    pub registered_name: Option<String>,
    /// Optional per-actor mailbox capacity override.
    pub mailbox_capacity: Option<usize>,
    /// Whether the runtime should link the child to the current actor.
    pub link_to_parent: bool,
    /// Whether the child should start with trap-exit enabled.
    pub trap_exit: bool,
    /// Parent actor metadata for supervision trees.
    pub parent: Option<ActorId>,
    /// Ancestor metadata for crash reports and observer views.
    pub ancestors: Vec<ActorId>,
    /// Stable supervisor child id when the spawn is owned by a supervisor.
    pub supervisor_child: Option<&'static str>,
}

/// Handle returned by `ask` for reply correlation and fast-path receives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingCall {
    /// The unique request reference.
    pub reference: Ref,
    /// The actor and reference that will receive the reply.
    pub reply_to: ReplyToken,
    /// Mailbox watermark captured when the call was created.
    pub mailbox_watermark: MailboxWatermark,
    /// Optional timeout for the request.
    pub timeout: Option<Duration>,
}

impl PendingCall {
    /// Creates a pending-call descriptor.
    pub const fn new(
        reply_to: ReplyToken,
        mailbox_watermark: MailboxWatermark,
        timeout: Option<Duration>,
    ) -> Self {
        Self {
            reference: reply_to.reference,
            reply_to,
            mailbox_watermark,
            timeout,
        }
    }

    /// Returns `true` when the reference matches the pending call.
    pub fn matches(self, reference: Ref) -> bool {
        self.reference == reference
    }
}

/// An envelope claimed from the current actor mailbox for this turn.
#[derive(Debug)]
pub struct ReceivedEnvelope(Envelope);

impl ReceivedEnvelope {
    pub(crate) const fn new(envelope: Envelope) -> Self {
        Self(envelope)
    }

    /// Borrows the claimed envelope without consuming it.
    pub fn as_envelope(&self) -> &Envelope {
        &self.0
    }

    /// Consumes the claimed envelope so the runtime can deliver it to `handle`.
    pub fn into_envelope(self) -> Envelope {
        self.0
    }
}

/// Runtime-managed timeout token for selective-receive wait states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReceiveTimeout {
    token: TimerToken,
}

impl ReceiveTimeout {
    /// Creates a receive-timeout handle from a timer token.
    pub const fn new(token: TimerToken) -> Self {
        Self { token }
    }

    /// Returns the underlying timer token.
    pub const fn token(self) -> TimerToken {
        self.token
    }

    /// Returns `true` when the envelope is this timeout firing.
    pub fn matches(self, envelope: &Envelope) -> bool {
        matches!(envelope, Envelope::Timer(timer) if timer.token == self.token)
    }
}

/// Handle for work dispatched onto a blocking pool.
///
/// Completion is routed back to the originating actor as an `Envelope::Task`
/// carrying the same reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskHandle<T> {
    id: Ref,
    pool: PoolKind,
    _marker: PhantomData<fn() -> T>,
}

impl<T> TaskHandle<T> {
    /// Creates a task handle from a reference and pool kind.
    pub const fn new(id: Ref, pool: PoolKind) -> Self {
        Self {
            id,
            pool,
            _marker: PhantomData,
        }
    }

    /// Returns the task id.
    pub const fn id(self) -> Ref {
        self.id
    }

    /// Returns the pool the task was submitted to.
    pub const fn pool(self) -> PoolKind {
        self.pool
    }
}

/// Send failure for async delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendError {
    /// The target actor does not exist.
    NoProc(ActorId),
    /// The target mailbox is applying backpressure.
    MailboxFull {
        /// Actor whose mailbox is full.
        actor: ActorId,
        /// Current bounded mailbox capacity.
        capacity: usize,
    },
}

/// Spawn failure for actor creation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnError {
    /// The runtime could not allocate an actor slot.
    CapacityExceeded,
    /// Initial registry registration was rejected.
    Registry(RegistryError),
    /// Actor initialization failed before the actor became runnable.
    InitFailed(ExitReason),
}

/// Link or unlink failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkError {
    /// The target actor does not exist.
    NoProc(ActorId),
    /// The link already exists.
    AlreadyLinked(ActorId),
    /// The link does not exist.
    NotLinked(ActorId),
}

/// Monitor or demonitor failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MonitorError {
    /// The target actor does not exist.
    NoProc(ActorId),
    /// The reference is not active.
    UnknownRef(Ref),
}

/// Timer scheduling or cancellation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimerError {
    /// The runtime cannot allocate another timer.
    CapacityExceeded,
    /// The timer reference is unknown or already expired.
    UnknownTimer(TimerToken),
}

/// Core runtime operations available to an actor while it is running.
pub trait ActorContext {
    /// Returns the current actor id.
    fn actor_id(&self) -> ActorId;

    /// Returns the current scheduler id when the actor is running on a scheduler.
    fn scheduler_id(&self) -> Option<usize> {
        None
    }

    /// Spawns a new actor into the runtime.
    fn spawn<A: Actor>(&mut self, actor: A, options: SpawnOptions) -> Result<ActorId, SpawnError>;

    /// Resolves a registered actor name through the runtime registry.
    fn whereis(&self, name: &str) -> Option<ProcessAddr>;

    /// Registers the current actor under a stable name.
    fn register_name(&mut self, name: String) -> Result<(), RegistryError>;

    /// Removes the current actor's registered name and returns it.
    fn unregister_name(&mut self) -> Option<String>;

    /// Sends an envelope directly to another actor.
    fn send_envelope<T: Into<ProcessAddr>>(
        &mut self,
        to: T,
        envelope: Envelope,
    ) -> Result<(), SendError>;

    /// Sends a typed request and captures the reply reference and mailbox watermark.
    ///
    /// When `timeout` is set, the runtime delivers `Envelope::CallTimeout` with
    /// the same reference if no reply arrives before the deadline.
    fn ask<M: Message, T: Into<ProcessAddr>>(
        &mut self,
        to: T,
        message: M,
        timeout: Option<Duration>,
    ) -> Result<PendingCall, SendError>;

    /// Returns a watermark representing envelopes that will arrive in the future.
    fn mailbox_watermark(&self) -> MailboxWatermark;

    /// Claims the next mailbox envelope using the runtime's default delivery rules.
    ///
    /// Only one envelope may be claimed per actor turn.
    fn receive_next(&mut self) -> Option<ReceivedEnvelope>;

    /// Claims a mailbox envelope that matches the predicate.
    ///
    /// Reserved runtime traffic may bypass the predicate so actors cannot hide
    /// critical control or failure signals behind selective receive.
    /// Only one envelope may be claimed per actor turn.
    fn receive_selective<F>(&mut self, predicate: F) -> Option<ReceivedEnvelope>
    where
        F: FnMut(&Envelope) -> bool;

    /// Claims the next envelope or a runtime-managed timeout firing.
    ///
    /// This helper is non-blocking. It returns `None` until either a mailbox
    /// envelope or the timeout token is available for the current turn.
    ///
    /// When the timeout fires, the returned envelope is `Envelope::Timer` with
    /// the same token stored in `timeout`.
    fn receive_next_with_timeout(&mut self, timeout: ReceiveTimeout) -> Option<ReceivedEnvelope> {
        self.receive_selective_with_timeout(timeout, |_| true)
    }

    /// Claims a mailbox envelope that matches the predicate or a timeout firing.
    ///
    /// The timeout is automatically cancelled when a non-timeout envelope wins.
    /// For wait loops that repeatedly scan large mailboxes, call `yield_now()`
    /// from `handle` after making progress so concurrent schedulers can rotate.
    fn receive_selective_with_timeout<F>(
        &mut self,
        timeout: ReceiveTimeout,
        mut predicate: F,
    ) -> Option<ReceivedEnvelope>
    where
        F: FnMut(&Envelope) -> bool,
    {
        let envelope =
            self.receive_selective(|envelope| timeout.matches(envelope) || predicate(envelope))?;

        if !timeout.matches(envelope.as_envelope()) {
            let _ = self.cancel_receive_timeout(timeout);
        }
        Some(envelope)
    }

    /// Claims a matching envelope whose mailbox sequence is at or after the watermark.
    ///
    /// Reserved runtime traffic may bypass the predicate so actors cannot hide
    /// critical control or failure signals behind selective receive.
    /// Only one envelope may be claimed per actor turn.
    fn receive_selective_after<F>(
        &mut self,
        watermark: MailboxWatermark,
        predicate: F,
    ) -> Option<ReceivedEnvelope>
    where
        F: FnMut(&Envelope) -> bool;

    /// Creates a bidirectional failure relationship with another actor.
    fn link<T: Into<ProcessAddr>>(&mut self, other: T) -> Result<(), LinkError>;

    /// Removes a bidirectional failure relationship.
    fn unlink<T: Into<ProcessAddr>>(&mut self, other: T) -> Result<(), LinkError>;

    /// Creates a one-way monitor and returns the reference.
    fn monitor<T: Into<ProcessAddr>>(&mut self, other: T) -> Result<Ref, MonitorError>;

    /// Removes a one-way monitor by reference.
    fn demonitor(&mut self, reference: Ref) -> Result<(), MonitorError>;

    /// Enables or disables trap-exit mode for the current actor.
    fn set_trap_exit(&mut self, enabled: bool);

    /// Schedules a timer for the current actor.
    fn schedule_after(&mut self, delay: Duration, token: TimerToken) -> Result<(), TimerError>;

    /// Cancels a previously scheduled timer.
    ///
    /// If the timer already fired but its mailbox envelope has not been
    /// observed yet, the runtime withdraws that queued timer envelope.
    fn cancel_timer(&mut self, token: TimerToken) -> bool;

    /// Arms a runtime-managed timeout for a selective-receive wait state.
    fn arm_receive_timeout(&mut self, delay: Duration) -> Result<ReceiveTimeout, TimerError> {
        let timeout = ReceiveTimeout::new(TimerToken::next());
        self.schedule_after(delay, timeout.token())?;
        Ok(timeout)
    }

    /// Cancels a runtime-managed receive timeout.
    fn cancel_receive_timeout(&mut self, timeout: ReceiveTimeout) -> bool {
        self.cancel_timer(timeout.token())
    }

    /// Dispatches blocking I/O work onto a dedicated pool.
    ///
    /// When the job completes, the runtime sends `Envelope::Task` back to the
    /// current actor.
    fn spawn_blocking_io<F, R>(&mut self, job: F) -> TaskHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static;

    /// Dispatches CPU-heavy work onto a dedicated pool.
    ///
    /// When the job completes, the runtime sends `Envelope::Task` back to the
    /// current actor.
    fn spawn_blocking_cpu<F, R>(&mut self, job: F) -> TaskHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static;

    /// Voluntarily yields the current scheduler after the current turn.
    fn yield_now(&mut self);

    /// Exits the current actor with the given reason.
    fn exit(&mut self, reason: ExitReason);

    /// Requests runtime-managed shutdown for another actor.
    fn shutdown_actor<T: Into<ProcessAddr>>(
        &mut self,
        actor: T,
        policy: Shutdown,
    ) -> Result<(), SendError>;
}

/// Higher-level actor spawn and messaging helpers layered on top of [`ActorContext`].
pub trait BehaviourContextExt: ActorContext {
    /// Spawns a typed `GenServer` into the runtime.
    fn spawn_gen_server<G>(
        &mut self,
        server: G,
        options: SpawnOptions,
    ) -> Result<ActorId, SpawnError>
    where
        G: GenServer,
        G::Info: From<RuntimeInfo>,
        G::State: Send,
    {
        self.spawn(GenServerActor::new(server), options)
    }

    /// Spawns a typed `GenStatem` into the runtime.
    fn spawn_gen_statem<G>(
        &mut self,
        machine: G,
        options: SpawnOptions,
    ) -> Result<ActorId, SpawnError>
    where
        G: GenStatem,
        G::Info: From<RuntimeInfo>,
    {
        self.spawn(GenStatemActor::new(machine), options)
    }

    /// Spawns a typed `Supervisor` into the runtime.
    fn spawn_supervisor<S>(
        &mut self,
        supervisor: S,
        options: SpawnOptions,
    ) -> Result<ActorId, SpawnError>
    where
        S: Supervisor,
    {
        self.spawn(SupervisorActor::new(supervisor), options)
    }

    /// Sends a typed fire-and-forget message to another actor.
    fn send<M: Message, T: Into<ProcessAddr>>(
        &mut self,
        to: T,
        message: M,
    ) -> Result<(), SendError> {
        self.send_envelope(to, Envelope::user(message))
    }

    /// Replies to a pending request.
    fn reply<M: Message>(&mut self, token: ReplyToken, message: M) -> Result<(), SendError> {
        self.send_envelope(token.from, Envelope::reply(token.reference, message))
    }
}

impl<T: ActorContext + ?Sized> BehaviourContextExt for T {}

/// Supervisor bookkeeping hooks layered on top of [`ActorContext`].
pub trait SupervisorContext: ActorContext {
    /// Registers supervisor metadata for the current actor.
    fn configure_supervisor(&mut self, _flags: SupervisorFlags, _child_specs: Vec<ChildSpec>) {}

    /// Records one supervisor restart attempt for the current actor.
    fn record_supervisor_restart(&mut self) -> bool {
        true
    }

    /// Marks a supervisor child as running in the current actor's runtime metadata.
    fn supervisor_child_started(&mut self, _child_id: &'static str, _actor: ActorId) {}

    /// Marks a supervisor child as exited in the current actor's runtime metadata.
    fn supervisor_child_exited(&mut self, _child_id: &'static str, _actor: ActorId) {}
}

/// Lifecycle event hooks layered on top of [`ActorContext`].
pub trait LifecycleContext: ActorContext {
    /// Emits a structured lifecycle event into the runtime stream.
    fn emit_lifecycle_event(&mut self, _event: LifecycleEvent) {}
}

/// Full actor runtime interface used by [`crate::Actor`].
pub trait Context:
    ActorContext + BehaviourContextExt + SupervisorContext + LifecycleContext
{
}

impl<T> Context for T where T: ActorContext + SupervisorContext + LifecycleContext {}
