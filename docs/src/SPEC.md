# lamport Functional Specification

## 1. Scope

This document specifies the functionality of an OTP-inspired, single-node actor
runtime. It is intended to be language-neutral. A conforming implementation may
be written in any language.

This document does not prescribe:

- source layout
- package or module names
- specific concurrency primitives
- specific standard libraries or dependencies
- exact public API syntax

It does prescribe:

- the runtime model
- the message and mailbox semantics
- the failure and supervision rules
- the observable lifecycle and metrics behavior

The current repository is a reference implementation of this specification, but
the repository structure itself is not normative.

## 2. Normative Language

The key words MUST, MUST NOT, SHOULD, SHOULD NOT, and MAY are used in their
usual normative sense.

## 3. System Overview

The system is a single-node actor runtime with these core features:

- isolated actors identified by stable actor IDs
- bounded mailboxes with FIFO delivery and selective receive
- synchronous, turn-based actor execution
- asynchronous request/reply with timeout delivery
- links, monitors, exit propagation, and trap-exit behavior
- supervisor trees with restart strategies and restart-intensity windows
- two runtime profiles:
  - a single-threaded local profile
  - a concurrent multi-scheduler profile
- observability through actor snapshots, topology, lifecycle events, crash
  reports, and Prometheus-style metrics export

The model is inspired by Erlang/OTP, but it is not BEAM-compatible and makes no
attempt to implement distributed node semantics.

## 4. Core Concepts

### 4.1 Actor Identity

Every live actor instance MUST have an actor ID with two logical parts:

- `slot`: a runtime-local numeric slot
- `generation`: a counter that distinguishes successive occupants of the same
  slot

An implementation MAY encode actor IDs however it likes, but it MUST preserve
these semantics:

- actor IDs are comparable and stable for the lifetime of an actor instance
- a restarted actor MUST receive a different actor ID from the prior instance
- stale actor IDs MUST NOT resolve to unrelated live actors

The reference implementation currently allocates only generation `0`, but the
conceptual model includes generation for stale-handle detection.

### 4.2 References

The runtime MUST provide a globally unique reference type used for:

- request/reply correlation
- monitor references
- timer tokens
- blocking-task handles

References MUST be monotonically allocated from a runtime-unique counter or an
equivalent source of uniqueness.

### 4.3 Timer Tokens

A timer token is a named reference used to correlate scheduled timers with
delivered timer events and cancellations.

Two timers scheduled by the same actor with the same timer token MUST be treated
as the same logical timer. Rescheduling such a timer MUST replace the previous
one.

## 5. Exit Reasons and Policies

### 5.1 Exit Reasons

Every actor termination MUST have one of these reasons:

- `Normal`
- `Shutdown`
- `Kill`
- `NoProc`
- `NoConnection`
- `Error(detail string)`

Intended meaning:

- `Normal`: clean completion
- `Shutdown`: ordered stop initiated by supervision or management logic
- `Kill`: forced, untrappable termination
- `NoProc`: target actor does not exist
- `NoConnection`: reserved for future distributed semantics
- `Error`: any abnormal failure or application-level crash

### 5.2 Restart Policy

Every supervised child specification MUST declare one of these restart policies:

- `Permanent`: always restart
- `Transient`: restart unless the child exited with `Normal` or `Shutdown`
- `Temporary`: never restart

### 5.3 Restart Strategy

Every supervisor MUST declare one of these subtree restart strategies:

- `OneForOne`: restart only the failed child
- `OneForAll`: restart all children
- `RestForOne`: restart the failed child and every child started after it

### 5.4 Shutdown Policy

Every supervised child specification MUST declare one of these shutdown
policies:

- `BrutalKill`: terminate immediately
- `Timeout(duration)`: request graceful shutdown, then force `Kill` if the child
  does not exit before the timeout
- `Infinity`: request graceful shutdown and wait indefinitely

The default shutdown policy MUST be `Timeout(5 seconds)`.

### 5.5 Supervisor Flags

Every supervisor MUST expose:

- restart strategy
- restart intensity limit
- restart intensity period

The default supervisor flags MUST be:

- strategy: `OneForOne`
- intensity: `3`
- period: `5 seconds`

### 5.6 Child Specification

Each child specification MUST contain:

- a stable child ID unique within the supervisor
- a restart policy
- a shutdown policy
- a boolean indicating whether the child is itself a supervisor

Child ID order is significant. It defines:

- supervisor start order
- reverse-order shutdown behavior
- `RestForOne` scope

## 6. Lifecycle and Metrics Model

### 6.1 Actor Status

Every actor snapshot MUST report one of these statuses:

- `Starting`
- `Runnable`
- `Waiting`
- `Running`
- `Exiting`
- `Dead`

### 6.2 Actor Metrics

Every actor snapshot MUST report:

- current mailbox length
- total turns executed
- total supervisor restarts recorded for that actor
- most recent exit reason, if any
- current scheduler assignment, if any

### 6.3 Lifecycle Events

The runtime MUST record lifecycle events with at least these variants:

- `Spawn`
- `Exit`
- `Down`
- `Restart`
- `Shutdown`

Each event MUST carry enough data to reconstruct the behavior observed in the
reference implementation:

- `Spawn`: actor ID, actor name, optional registered name, optional parent,
  optional supervisor child ID
- `Exit`: actor ID, actor name, final reason, optional parent, ancestor chain
- `Down`: watcher actor, target actor, monitor reference, observed reason
- `Restart`: supervisor actor, child ID, old actor if any, new actor
- `Shutdown`: requester actor, target actor, shutdown policy, phase, optional
  reason

Shutdown phases MUST be:

- `Requested`
- `TimedOut`
- `Completed`

### 6.4 Crash Reports

The runtime MUST emit a crash report for every abnormal actor exit.

An exit is abnormal if its reason is neither `Normal` nor `Shutdown`.

Each crash report MUST include:

- the crashing actor
- the actor's human-readable name
- optional registered name
- optional parent actor ID
- resolved parent identity if still available
- the actor's ancestor chain
- resolved ancestor identities if still available
- optional supervisor child ID
- final exit reason

## 7. Message Model

### 7.1 Payloads

The runtime MUST support dynamically typed message payloads.

Each payload MUST retain:

- the underlying application value
- a stable type label or equivalent human-readable type description captured at
  send time

That type label is required for:

- typed behavior adapters
- debugging
- error reporting for unexpected payloads

An implementation MAY represent this as:

- a boxed dynamic value with reflection metadata
- an interface value plus type descriptor
- a tagged union
- any equivalent mechanism

### 7.2 Inline Message Guidance

The runtime MUST publish a recommended inline message limit of 64 KiB.

This is only guidance. Mailbox admission is based on envelope count, not byte
 size. Large immutable payloads SHOULD be moved into shared buffers or an
 equivalent zero-copy representation when appropriate.

### 7.3 Reply Tokens

A reply token identifies where a call reply must be delivered. It contains:

- the calling actor
- the unique request reference

### 7.4 Runtime Message Types

The runtime MUST support these runtime-originated message types:

- exit signal: `{from, reason, linked}`
- down message: `{reference, actor, reason}`
- timer fired: `{token}`
- call timed out: `{reference}`
- task completed: `{reference, pool, result}`

### 7.5 System Messages

The runtime MUST reserve a namespace for system messages with at least these
variants:

- `Suspend`
- `Resume`
- `GetState`
- `ReplaceState(payload)`
- `TraceOn`
- `TraceOff`
- `Shutdown`
- `CodeChange`

Current required behavior:

- the mailbox reserve rules MUST treat system messages as runtime-originated
  control messages
- supervisors MUST interpret `Shutdown`
- typed behaviors MUST surface system messages to their `info` path
- the remaining system messages are reserved for future expansion and need not
  have built-in semantics yet

### 7.6 Envelope Kinds

Every delivered envelope MUST have one of these kinds:

- `User`
- `Request`
- `Reply`
- `Task`
- `CallTimeout`
- `Exit`
- `Down`
- `Timer`
- `System`

Kinds `Reply`, `Task`, `CallTimeout`, `Exit`, `Down`, `Timer`, and `System`
MUST be treated as runtime-originated for mailbox reserve purposes.

### 7.7 Envelope Forms

The runtime MUST support these envelope forms:

- user envelope carrying an application payload
- request envelope carrying a reply token plus an application payload
- reply envelope carrying a request reference plus an application payload
- task envelope carrying a completed task record
- call-timeout envelope carrying a timed-out request reference
- exit envelope carrying an exit signal
- down envelope carrying a monitor notification
- timer envelope carrying a timer token
- system envelope carrying a system message

## 8. Mailbox Semantics

### 8.1 Mailbox Model

Each actor has one mailbox.

The mailbox MUST:

- preserve FIFO order by default
- support selective receive
- support watermark-based selective receive that ignores older messages
- be bounded by envelope count, not payload size

Each enqueued envelope MUST receive a monotonically increasing mailbox sequence
number.

A mailbox watermark represents the next sequence number that would be assigned.

### 8.2 Mailbox Configuration

Every mailbox has:

- a capacity
- a runtime-reserve count

Rules:

- capacity MUST be at least `1`
- runtime reserve MUST be clamped to at most `capacity - 1`
- an unbounded mailbox MAY be modeled as effectively infinite capacity

### 8.3 Admission Rules

For user-originated envelope kinds (`User`, `Request`):

- admission MUST fail once `len >= capacity - runtime_reserve`

For runtime-originated envelope kinds (`Reply`, `Task`, `CallTimeout`, `Exit`,
`Down`, `Timer`, `System`):

- admission MUST fail once `len >= capacity`

This guarantees that runtime-originated control traffic can still be delivered
after user traffic has filled the non-reserved portion of the mailbox.

### 8.4 Operations

The mailbox MUST support:

- enqueue
- enqueue with explicit backpressure error
- pop oldest
- selective receive by predicate
- selective receive after watermark by predicate

Selective receive rules:

- it removes the first matching envelope
- unmatched envelopes retain relative order
- watermark-based selective receive MUST ignore all envelopes older than the
  watermark even if they match the predicate

## 9. Actor Contract

### 9.1 Turn-Based Execution

An actor is a stateful entity with three lifecycle hooks:

- initialize
- handle one envelope
- terminate

Actor execution MUST be synchronous and turn-based:

- one turn processes at most one delivered envelope
- an actor MUST NOT execute more than one turn concurrently
- async workflows MUST be modeled through message passing, timers, or blocking
  task completions routed back through the mailbox

### 9.2 Turn Outcome

Each handled turn MUST produce one of these outcomes:

- continue
- yield
- stop with a reason

Meaning:

- `continue`: actor remains alive and may process more work
- `yield`: actor remains alive but requests a scheduling yield
- `stop`: actor terminates with the provided reason

### 9.3 Actor Name

Each actor SHOULD expose a human-readable name for:

- tracing
- crash reporting
- observability snapshots

How the name is chosen is implementation-specific. A type or class name is a
reasonable default.

## 10. Runtime Services Available to Actors

Every running actor MUST have access to a context or equivalent runtime handle
providing the following capabilities.

### 10.1 Spawn

Actors MUST be able to spawn child actors with spawn options containing:

- optional registered name
- optional mailbox capacity override
- whether to link to the parent
- whether trap-exit starts enabled
- optional parent actor metadata
- ancestor metadata
- optional supervisor child ID metadata

Built-in spawn semantics MUST be:

- if parent metadata is absent, parent defaults to the current actor
- if ancestor metadata is empty, it becomes the current actor's ancestors plus
  the current actor
- if `link_to_parent` is enabled, a bidirectional link is created
- if a registered name is provided, registration happens before the actor becomes
  live

### 10.2 Name Lookup and Registration

Actors MUST be able to:

- resolve an actor by registered name
- register themselves under a stable name
- unregister their current name

### 10.3 Send

Actors MUST be able to:

- send a raw envelope
- send a user message as a convenience wrapper around a user envelope

Sending to a missing actor MUST fail with a no-process error.

Sending to a full mailbox MUST fail with an explicit mailbox-full error carrying
the target actor and capacity.

### 10.4 Ask / Reply

Actors MUST be able to perform synchronous request/reply messaging:

- allocate a fresh request reference
- create a reply token pointing back to the caller
- capture the caller's mailbox watermark before sending the request
- optionally arm a timeout for that request

The returned pending-call handle MUST include:

- the request reference
- the reply token
- the captured mailbox watermark
- the timeout value, if any

Replies MUST be delivered as reply envelopes addressed to the original caller.

Actors MUST also be able to arm a runtime-managed receive timeout for
actor-selected receive loops.

Receive-timeout helper semantics:

- arm a fresh timer token managed by the runtime
- allow selective receive to match either the user predicate or that timeout
  token
- cancel and withdraw the queued timeout envelope when a non-timeout envelope
  wins first
- leave timeout handling observable as an ordinary timer envelope carrying that
  token

### 10.5 Link / Unlink

Actors MUST be able to create and remove bidirectional links.

Link semantics:

- linking to self is a no-op that succeeds
- unlinking self is a no-op that succeeds
- linking to a missing actor fails with no-process
- linking to an already-linked actor fails with already-linked
- unlinking a non-linked actor fails with not-linked

### 10.6 Monitor / Demonitor

Actors MUST be able to create one-way monitors and remove them by reference.

Monitor semantics:

- monitoring self succeeds and creates matching incoming and outgoing monitor
  entries on the same actor
- monitoring a missing actor MUST succeed and enqueue an immediate down message
  with reason `NoProc`
- demonitor of an unknown reference fails with unknown-reference

### 10.7 Trap Exit

Actors MUST be able to toggle trap-exit mode.

Trap-exit controls how linked exits are handled:

- non-trapping actors are terminated by abnormal linked exits
- trapping actors receive exit envelopes instead

### 10.8 Timers

Actors MUST be able to:

- schedule a timer after a delay using a timer token
- cancel a previously scheduled timer token

Scheduling the same token again for the same actor MUST replace the prior timer.

### 10.9 Blocking Work

Actors MUST be able to dispatch blocking work onto:

- a blocking-I/O pool
- a blocking-CPU pool

Completion MUST be routed back into the actor mailbox as a task-completed
envelope carrying:

- the task reference
- the pool kind
- the task result payload

### 10.10 Voluntary Yield

Actors MUST be able to request a voluntary yield after the current turn.

The single-threaded runtime profile MAY treat this as having no externally
observable effect.

The concurrent runtime profile MUST honor it by yielding scheduler time after
the current turn.

Actor code that stays in a selective-receive wait state across many turns
SHOULD request voluntary yield periodically after making progress so large
mailbox scans do not monopolize a scheduler.

### 10.11 Exit Self

Actors MUST be able to request that they terminate after the current turn with a
specific reason.

### 10.12 Supervisor Hooks

The runtime MUST expose hooks used internally by supervisors to:

- configure supervisor metadata on the supervisor actor
- record a restart attempt against restart intensity
- mark a supervisor child as started
- mark a supervisor child as exited
- emit explicit lifecycle events

These hooks are part of the runtime contract even if they are not exposed as a
general-purpose public API in every language binding.

### 10.13 Shutdown Another Actor

Actors MUST be able to request runtime-managed shutdown of another actor using a
shutdown policy.

## 11. Error Categories

The runtime MUST expose or internally model these error categories:

### 11.1 Send Errors

- target actor does not exist
- target mailbox is full

### 11.2 Spawn Errors

Immediate spawn rejection MUST cover:

- actor-capacity exhaustion
- initial name-registration rejection

Important semantic rule:

- actor initialization failure is asynchronous in the built-in runtime profiles
- a spawn request that has already returned an actor ID MUST NOT later be
  reported as an immediate spawn error
- instead, the actor exits during or after initialization and is observed through
  normal lifecycle events and snapshots

### 11.3 Link Errors

- missing target
- already linked
- not linked

### 11.4 Monitor Errors

- missing target only if the API chooses to reject it; the built-in profiles do
  not reject it and instead enqueue an immediate down message
- unknown monitor reference on demonitor

### 11.5 Timer Errors

The model includes:

- capacity exceeded
- unknown timer

The reference implementation currently does not need a hard timer-capacity
limit, so capacity-exceeded is reserved rather than actively exercised.

## 12. Failure Propagation and Shutdown

### 12.1 Links

When actor `A` exits, every linked actor `B` MUST be handled as follows:

- if the reason is `Kill`, `B` is terminated immediately
- if the link is ordinary and `B` is not trapping exits:
  - `Normal` is ignored
  - any non-`Normal` reason terminates `B`
- if `B` is trapping exits:
  - deliver an exit envelope carrying `{from=A, reason, linked=true}`

### 12.2 Monitors

When a monitored actor exits:

- every watcher receives one down envelope
- the watcher's outgoing monitor entry is removed
- the target's incoming monitor entry is removed
- a `Down` lifecycle event is emitted

### 12.3 Runtime-Originated Delivery Failures

If the runtime attempts to deliver a runtime-originated envelope and the mailbox
is still full even after using the reserve, the target actor MUST be forcibly
terminated with an error reason indicating mailbox overflow while delivering the
relevant runtime label.

This rule applies to delivery of:

- replies
- task completions
- call timeouts
- exit signals
- down notifications
- timer events
- system messages

### 12.4 Forced Exit

The runtime MUST support immediate forced exit of a live actor.

Forced exit bypasses graceful shutdown and directly begins actor finalization.

### 12.5 Managed Shutdown

Managed shutdown of target actor `T` requested by actor `R` MUST work as
follows:

1. If `T` does not exist, fail with no-process.
2. Cancel any existing managed shutdown in progress for `T`.
3. Emit `Shutdown { requester=R, actor=T, phase=Requested, policy, reason=nil }`.
4. Apply the shutdown policy:
   - `BrutalKill`: force-exit `T` with final reason `Kill`
   - `Timeout(d)`: begin graceful shutdown and arm a shutdown timeout
   - `Infinity`: begin graceful shutdown without a timeout

Graceful shutdown means:

- propagate a linked exit carrying reason `Shutdown`
- let the target exit on its own if it traps exits and handles the shutdown
- otherwise allow normal link semantics to terminate it immediately

If the shutdown timeout fires first:

- emit `Shutdown { phase=TimedOut, reason=Kill }`
- force-exit the target with final reason `Kill`

When the target finally exits:

- emit `Shutdown { phase=Completed, reason=final_reason }`

### 12.6 Observed Reason Split During Managed Shutdown

Managed shutdown intentionally distinguishes two views of the same stop:

- linked peers observe shutdown semantics
- monitors and crash reports observe the actor's actual final exit reason

Example:

- a timed-out shutdown may cause linked peers to see `Shutdown`
- but monitors and crash reports see `Kill`

This distinction is required.

### 12.7 Actor Finalization

When an actor exits for any reason, the runtime MUST:

- mark the actor exiting
- invoke the actor's terminate hook exactly once
- unregister the actor's registered name
- cancel all timers owned by the actor
- cancel all request timeouts owned by the actor
- remove outgoing monitors
- notify incoming monitors
- detach links and propagate linked exits
- clear supervisor-child runtime metadata on the parent if applicable
- retain a completed snapshot for later lookup
- update actor metrics with the last exit reason

Completed snapshots MUST remain queryable by actor ID after death.

## 13. Registry Semantics

The runtime registry is a one-name-per-actor map with two indexes:

- name -> actor
- actor -> name

Rules:

- registering the same actor under the same name is idempotent
- registering an actor that already owns a different name fails
- registering a name already owned by another actor fails
- unregistering removes both indexes and returns the old name
- runtime-level registration MUST reject actors that are not live

## 14. Runtime Profiles

The runtime MUST provide two execution profiles with identical functional
semantics and different scheduling behavior.

### 14.1 Shared Configuration

The runtime configuration model MUST include:

- scheduler count
- maximum live actors
- default mailbox capacity
- mailbox runtime reserve
- local run-queue capacity
- inject-queue capacity
- work-steal batch size
- actor turn budget
- blocking-I/O worker count
- blocking-CPU worker count

Default values MUST be:

- scheduler count: available CPUs, or `1` if unknown
- max actors: `65_536`
- default mailbox capacity: `1_024`
- mailbox runtime reserve: `32`
- local run-queue capacity: `4096`
- inject-queue capacity: `16384`
- steal batch size: `64`
- actor turn budget: `64`
- blocking-I/O workers: `max(scheduler_count, 4)`
- blocking-CPU workers: `max(scheduler_count, 2)`

Current functionality only depends materially on:

- max actors
- mailbox defaults and reserve
- scheduler count
- actor turn budget
- blocking worker counts

The queue-capacity and steal-batch fields are reserved for future scheduling
work and need not affect runtime behavior yet.

### 14.2 Single-Threaded Local Profile

The local profile MUST normalize configuration so that scheduler count becomes
`1`.

It MUST expose explicit driver operations:

- spawn actor
- send
- force exit
- run one step
- run until idle
- block waiting for the next external event

Execution model:

- spawning inserts the actor immediately and returns its actor ID before init
  runs
- actors start in status `Starting`
- each call to `run one step` first drains external events, then executes at
  most one actor action
- initialization is itself a scheduled action
- a normal actor turn pops exactly one envelope
- if no work was done and no external event fired, increment the idle-turn count

Blocking wait semantics:

- if immediate progress is possible, do not block
- otherwise wait for the next external event up to the requested timeout
- if an event arrives, ingest it, then execute at most one resulting actor step

Voluntary yield in this profile MAY be ignored as a scheduling primitive. The
reference implementation does so.

### 14.3 Concurrent Multi-Scheduler Profile

The concurrent profile MUST normalize configuration so that:

- scheduler count >= 1
- blocking-I/O workers >= 1
- blocking-CPU workers >= 1
- actor turn budget >= 1

Execution model:

- each actor has a long-lived driver task or equivalent worker loop
- different actors MAY run in parallel on different schedulers
- one actor MUST still remain strictly serialized
- actor scheduler assignment MUST be stable enough to report in snapshots
- after `yield` or after `actor_turn_budget` back-to-back turns, the runtime
  MUST yield scheduler time

The concurrent profile MUST provide an `idle` wait primitive that returns true
only when every live actor:

- has an empty mailbox
- is in `Waiting` or `Dead`

## 15. Request Timeouts, Timers, and Blocking Work

### 15.1 Request Timeout Semantics

Every pending request timeout is keyed by:

- calling actor
- request reference

If a reply is enqueued before the timeout fires:

- cancel the timeout

If the timeout fires first:

- deliver a call-timeout envelope to the caller

Late replies are NOT suppressed. They are still delivered as ordinary replies if
they arrive after the timeout envelope.

### 15.2 Timer Semantics

Every timer is keyed by:

- target actor
- timer token

If a timer fires and still exists:

- remove it from the timer table
- deliver a timer envelope

If a timer was cancelled or replaced first:

- its eventual wakeup MUST be ignored

### 15.3 Blocking Work Semantics

Blocking work MUST NOT block actor-turn execution.

Each submitted blocking job MUST:

- allocate a task reference
- run on the selected blocking pool
- return exactly one task-completed envelope to the originating actor when done

The runtime metrics MUST count blocking-I/O jobs and blocking-CPU jobs
separately.

## 16. Typed Behavior Adapters

The runtime MUST provide higher-level typed behavior adapters equivalent to OTP
`gen_server` and `gen_statem`.

The exact API shape is implementation-specific. The semantics are not.

### 16.1 Typed Server Behavior

A typed server owns:

- mutable server state
- a call message type
- a cast message type
- a reply message type
- an info message type

It MUST support:

- initialization returning the initial state
- handling synchronous calls
- handling asynchronous casts
- handling informational messages
- termination callback
- reserved code-change callback

Call outcomes MUST include:

- reply and continue
- reply and stop
- no reply and continue
- stop without reply

Cast/info outcomes MUST include:

- continue
- yield
- stop

Adapter semantics:

- request envelopes are routed to the call handler after payload-type validation
- user envelopes are routed to either cast or explicit user-info handling after
  payload-type validation
- runtime-originated envelopes are converted into info messages
- wrong payload types MUST stop the actor with an error reason that mentions the
  unexpected payload type label
- reply-producing outcomes MUST automatically send the reply

### 16.2 Typed State-Machine Behavior

A typed state machine owns:

- a current state identifier
- mutable data carried across transitions
- a call message type
- a cast message type
- a reply message type
- an info message type

It MUST support:

- initialization returning `(state, data)`
- call handling that may reply and/or transition
- cast handling that may transition
- info handling that may transition
- termination callback
- reserved code-change callback

Required call outcomes:

- reply
- reply and transition
- no reply
- no reply and transition
- reply and stop
- stop

Required cast/info outcomes:

- continue
- transition
- yield
- transition and yield
- stop

The adapter MUST apply transitions before the next turn begins.

### 16.3 Reserved Code-Change Hook

Both typed behavior families MUST expose a reserved code-change hook.

Current functionality does not require the runtime to invoke it yet.

## 17. Supervision

### 17.1 Supervisor Contract

A supervisor owns:

- supervisor flags
- ordered child specifications
- a child-start routine
- a child-exit decision routine

When wrapped as a runnable actor, the supervisor MUST:

- trap exits
- configure supervisor metadata in the runtime
- start all declared children in order during initialization

### 17.2 Child Start Semantics

Starting a child MUST occur through a supervised child-spawn context that forces:

- link-to-parent enabled
- supervisor-child ID metadata set to the child's declared ID

If the supervisor's child-start routine returns an actor other than the one it
spawned through the provided child context, the runtime MUST treat that as spawn
rejection.

Child-start error categories MUST include:

- spawn rejected
- already started
- init failed

### 17.3 Supervisor Initialization Failure

If any child fails to start during supervisor initialization:

- brutally kill any already-started children
- fail supervisor initialization with an error reason derived from the child
  start failure

### 17.4 Restart Scope

Given child order `[c1, c2, ... cn]` and failed child `ck`:

- `OneForOne` restart scope is `[ck]`
- `OneForAll` restart scope is `[c1..cn]`
- `RestForOne` restart scope is `[ck..cn]`

If the failed child is not found, the restart scope is empty.

### 17.5 Restart Intensity

The supervisor MUST maintain a sliding window of restart timestamps.

Rules:

- prune any restart older than the configured period
- record the new restart
- the restart is allowed only if active restarts <= configured intensity

If the limit is exceeded:

- the supervisor MUST shut down with error detail
  `supervisor restart intensity exceeded`

### 17.6 Handling Child Exit

When a tracked child exits:

- remove its actor mapping
- update runtime supervisor-child metadata
- if a restart or shutdown plan is already in progress, advance that plan
- otherwise ask the typed supervisor what to do next

Possible directives:

- ignore
- restart a set of child IDs
- shut down the subtree

### 17.7 Restart Plan

To restart a set of child IDs:

1. Normalize the child ID list into declared child order.
2. If the set is empty, do nothing.
3. Record a restart attempt against restart intensity.
4. If intensity is exceeded, shut down the supervisor.
5. Shut down any still-running children in the restart set in reverse declared
   order.
6. Once they are gone, start replacement children in forward declared order.
7. Emit a `Restart` lifecycle event for each successfully restarted child.

If any replacement child fails to start:

- shut down the supervisor with an error describing the failed child

### 17.8 Shutdown Plan

To shut down a supervisor subtree:

1. Collect all currently running children in reverse declared order.
2. Request shutdown of one child at a time.
3. When the current child exits, proceed to the next.
4. When all children are gone, stop the supervisor with the final shutdown
   reason.

### 17.9 External Linked Exit Into a Supervisor

If a supervisor receives a linked exit from an actor that is not one of its
tracked children:

- if the supervisor is already shutting down, ignore the extra exit
- otherwise begin subtree shutdown using that exit reason

### 17.10 Required Observable Behavior

A conforming implementation MUST satisfy these supervisor behaviors:

- `OneForOne` restarts only the failed child
- `OneForAll` restarts the whole subtree
- `RestForOne` restarts only the failed suffix
- shutdown requests proceed in reverse declared child order
- timed-out child shutdown emits a timed-out shutdown lifecycle event and then
  forces `Kill`
- restart intensity overflow terminates the supervisor and the remaining running
  subtree

## 18. Application Boot

The runtime MUST support an application-level boot contract:

- an application has a human-readable name
- an application provides root-supervisor spawn options
- an application builds a root supervisor

Booting an application MUST:

- create or reuse a runtime
- spawn the root supervisor with the application's root options
- return an application handle containing:
  - application name
  - root supervisor actor ID

The exact API shape is implementation-specific.

## 19. Observability

### 19.1 Actor Snapshot

Every live or retained actor snapshot MUST include:

- actor ID
- human-readable name
- optional registered name
- optional parent actor
- ancestor chain
- current link set
- incoming monitor set
- outgoing monitor set
- trap-exit flag
- status
- actor metrics

### 19.2 Supervisor Snapshot

Every live supervisor snapshot MUST include:

- supervisor actor ID
- supervisor flags
- ordered child list
- for each child:
  - child specification
  - currently running actor, if any
- active restart count within the current intensity window

### 19.3 Actor Identity

The observability layer MUST provide a reusable identity object containing:

- actor ID
- actor name
- optional registered name

### 19.4 Event Log

The runtime MUST retain a structured event log.

Each event MUST include:

- a monotonically increasing sequence number starting at `1`
- an emission timestamp
- an event kind

The event log MUST support incremental consumption using a cursor:

- a cursor from start begins before sequence `1`
- a cursor created from the current runtime position begins after the current
  tail
- `events since cursor` returns all events with sequence >= cursor.next
- after returning events, advance the cursor to one past the returned tail

### 19.5 Actor Tree

The observability layer MUST expose a live actor tree:

- each node is a live actor snapshot plus its direct live children
- a root is any live actor whose parent is absent from the live snapshot set
- child order and root order MUST be stable and sorted by actor ID or an
  equivalent total ordering

### 19.6 Metrics Snapshot

The runtime MUST expose an aggregate metrics snapshot containing:

- capture time
- live actor count
- retained completed actor count
- counts of actors in each lifecycle state
- total mailbox length across live actors
- maximum live mailbox length
- scheduler metrics
- per-scheduler queue snapshots

### 19.7 Scheduler Metrics

Scheduler metrics MUST include:

- utilization ratio
- total normal turns
- total idle turns
- total blocking-I/O jobs
- total blocking-CPU jobs

Queue snapshots MUST include:

- scheduler ID
- runnable count
- waiting count
- injected count
- stolen count

If a scheduler implementation does not yet model injected or stolen work, it
MAY report zero for those fields.

### 19.8 Prometheus Export

The metrics snapshot MUST be exportable in Prometheus text exposition format
using these metric families:

- `lamport_runtime_observed_at_seconds`
- `lamport_runtime_live_actors`
- `lamport_runtime_completed_actors`
- `lamport_runtime_actor_status{status=...}`
- `lamport_runtime_mailbox_messages`
- `lamport_runtime_mailbox_max_messages`
- `lamport_scheduler_utilization_ratio`
- `lamport_scheduler_normal_turns_total`
- `lamport_scheduler_idle_turns_total`
- `lamport_scheduler_blocking_io_jobs_total`
- `lamport_scheduler_blocking_cpu_jobs_total`
- `lamport_scheduler_runnable_actors{scheduler_id=...}`
- `lamport_scheduler_waiting_actors{scheduler_id=...}`
- `lamport_scheduler_injected_total{scheduler_id=...}`
- `lamport_scheduler_stolen_total{scheduler_id=...}`

## 20. Conformance Requirements

An implementation conforms to this specification if it reproduces these
externally visible behaviors:

- FIFO mailbox delivery, selective receive, and watermark filtering
- bounded mailbox backpressure for user traffic
- runtime-reserve handling for replies, timeouts, timers, down messages, exits,
  tasks, and system messages
- request/reply with timeout delivery to the caller
- timer delivery by token
- completion of blocking work through the mailbox
- strict per-actor serialization
- link semantics:
  - abnormal linked exits kill non-trapping peers
  - trapping peers receive exit messages instead
  - `Normal` linked exits are ignored by non-trapping peers
- monitor semantics:
  - immediate `Down(NoProc)` on monitoring a missing actor
  - down delivery on actor exit
- retained completed actor snapshots
- crash reports with parent and ancestor context when available
- local single-threaded driving behavior
- concurrent multi-actor parallelism with per-actor serialization preserved
- supervisor restart strategies, shutdown ordering, and restart intensity
- application boot through a root supervisor
- actor topology and Prometheus metrics export

## 21. Non-Goals and Reserved Surface

This specification intentionally does not require:

- distributed node connectivity
- BEAM compatibility
- byte-size-based mailbox admission control
- a direct runtime-level selective-receive API beyond the mailbox abstraction
- an active code-change pipeline, despite reserving the hook
- immediate use of run-queue-capacity and steal-batch configuration fields

These are reserved for future expansion but are not part of current required
functionality.
