# Lamport TODO

This file captures the next set of runtime changes needed to move Lamport
toward a more BEAM-like OTP runtime without aiming for full BEAM
compatibility.

The work is split into two regimes:

- things worth doing while Lamport remains explicitly single-node
- things that become necessary once Lamport grows into a multi-node runtime

## 1. Single-Node Only Regime

Goal: make the local runtime model stronger first, especially around control,
mailbox semantics, and process identity, without committing to any network or
cluster protocol yet.

### 1.1 Harden Local Identity And Addressing

- Make generation-bearing actor ids fully real, including safe slot reuse and
  stale-handle rejection.
- Tighten `Ref` semantics so request refs, monitor refs, timer refs, and task
  refs have one obvious lifecycle and uniqueness story.
- Introduce a process-address abstraction that still points only to local
  actors for now, but avoids baking "always local" assumptions into every API.
- Keep the registry explicitly local and fast; do not mix in distributed naming
  semantics yet.

Why this matters:

- It lets the single-node runtime become more precise immediately.
- It reduces how much API churn is needed later if remote process addressing is
  added.

### 1.2 Make The Control Plane Real

Goal: turn the currently reserved system/control surface into an explicit,
runtime-owned local protocol for inspection, coordination, tracing, shutdown,
and code change.

- Define which messages are true system messages.
  - `Suspend`, `Resume`, `GetState`, `ReplaceState`, `TraceOn`, `TraceOff`,
    `Shutdown`, and `CodeChange` need runtime-level semantics rather than
    "just another enum variant".
  - Decide which of these bypass normal user backpressure and whether they have
    priority over user mail.

- Add a real system-message path.
  - Deliver control messages through a reserved lane or explicit priority path.
  - Prevent user traffic from indefinitely starving suspend/shutdown/control
    operations.
  - Make the behavior consistent in both local and concurrent runtimes.

- Implement suspend/resume semantics.
  - Suspending a process should stop normal message execution without losing
    mailbox contents.
  - Define whether exits, downs, and other critical signals still flow while
    suspended.
  - Decide how a supervisor behaves when a child is suspended.

- Implement process inspection and local state extraction.
  - `GetState` should have a well-defined reply shape and failure mode for
    actors that cannot or will not expose state.
  - Tracing should be explicit about what gets recorded: receives, sends,
    lifecycle events, scheduler assignment, mailbox depth, or all of the above.

- Implement controlled state replacement and code change.
  - `ReplaceState` should validate the replacement payload and fail closed.
  - `CodeChange` should call the reserved behavior hooks through a real runtime
    flow, not an ad hoc manual call.
  - State migration needs versioning so code upgrades can reject incompatible
    state cleanly.

- Tighten shutdown semantics.
  - System-initiated shutdown should share one implementation path with
    supervisor and external shutdown requests.
  - Define what is guaranteed to run during orderly stop: terminate hooks,
    mailbox draining, final reports, and monitor/link notifications.

- Add control-plane observability and tests.
  - Test suspend/resume under load, shutdown during mailbox pressure, code
    change during pending calls, and tracing overhead.

Why this matters:

- It improves debuggability and operability even if Lamport never becomes
  distributed.
- It creates the local semantics that a future distributed control plane would
  need to forward or authorize.

### 1.3 Add Real Process-Side Receive Semantics

Goal: let an actor decide what it is waiting for, scan its mailbox selectively,
and block until a matching message or timeout arrives, instead of always being
handed the next envelope by the runtime.

- Introduce a receive-oriented runtime API.
  - Add process-side operations for "receive next", selective receive, and
    selective receive after watermark.
  - Preserve the ability to express timeouts without forcing every caller to
    build its own timer protocol.
  - Keep the API usable from both raw actors and higher-level behaviors.

- Move mailbox choice closer to the actor.
  - Today the runtime pops one envelope and invokes `handle`.
  - A receive-oriented model means the actor participates in choosing which
    queued message to consume next.
  - That likely requires changing the low-level actor contract, not just adding
    helper methods to `Context`.

- Define unmatched-message behavior.
  - Selective receive needs a save-queue or equivalent policy so unmatched
    messages are preserved in order.
  - Document the performance cost of mailbox scanning and the fairness tradeoff
    under large mailboxes.

- Specify interaction with system traffic.
  - Decide whether system/control messages bypass user selective receive,
    interleave with it, or are exposed through a separate receive path.
  - Exit, down, and shutdown signals must not become unobservable because user
    code is waiting for a different payload shape.

- Reconcile receive semantics with typed behaviors.
  - `GenServer` and `GenStatem` can stay structured APIs, but their
    implementations need to coexist with raw selective receive.
  - Pending-call reply correlation should be expressible as a receive pattern
    rather than a side-channel abstraction only.

- Revisit scheduler and mailbox invariants.
  - A receive loop can stay single-process-serialized, but it changes how long
    a process may hold control while scanning or waiting.
  - The runtime needs clear yield points so one process cannot monopolize a
    scheduler by repeated mailbox scans.

- Add thorough tests.
  - Cover watermark-based selective receive, timeout races, late replies, exit
    delivery while waiting, and fairness under mailbox growth.

Why this matters:

- This is the biggest semantic step toward OTP-style process behavior even in a
  purely local runtime.
- It should be proven locally before adding network-induced races and failure
  modes.

## 2. Multi-Node Regime

Goal: once the local runtime model is solid, give a Lamport runtime instance a
stable node identity and let nodes exchange messages, signals, and control
traffic.

### 2.1 Define Node Identity

- Add a first-class `NodeId`/`NodeName` type.
- Include a creation/incarnation counter so a restarted node does not reuse
  stale identities.
- Decide whether node naming is static config, negotiated at boot, or both.

### 2.2 Make Process Identity Node-Aware

- Extend the process-address abstraction to represent `local slot + generation
  + node`.
- Keep stale-handle protection for restarted local actors and restarted remote
  nodes.
- Extend `Ref` semantics so monitor refs and request refs remain unique across
  nodes.

### 2.3 Add A Transport And Connection Layer

- Establish node-to-node connections over a concrete transport.
- Define handshake, authentication, version negotiation, heartbeat, and
  reconnect behavior.
- Decide whether the first version is full mesh, manually configured peers, or
  a simpler static cluster model.

### 2.4 Support Remote Messaging And Signals

- Send ordinary messages, replies, exit signals, down notifications, and
  timer-like control traffic across nodes.
- Preserve per-sender ordering where possible and document where ordering stops
  at connection boundaries.
- Make `NoConnection` a real runtime outcome rather than a reserved reason.

### 2.5 Extend Links And Monitors Across Nodes

- Remote monitors should produce `Down(NoConnection)` or equivalent node-down
  signals on partition or disconnect.
- Remote links need explicit node-failure semantics, not just process-exit
  semantics.
- Disconnect handling must avoid delivering stale signals after reconnect to a
  different node incarnation.

### 2.6 Add A Distributed Naming Story

- Keep the existing local registry for local fast-path lookups.
- Add a distinct distributed naming story only after transport semantics are
  stable.
- Do not overload the current local registry API with global semantics until
  partition behavior is defined.

### 2.7 Extend The Control Plane Across Nodes

- Decide which control-plane actions can be invoked remotely.
- Add authorization boundaries so a connected node is not automatically allowed
  to suspend, trace, or replace state everywhere.
- Define whether remote `GetState`/trace/shutdown flow through the same local
  system-message path or through a separate gateway layer.

### 2.8 Add Topology And Fault-Injection Tests

- Cover disconnect, reconnect, netsplit, duplicate reconnect, and stale pid
  cases.
- Verify monitor/link behavior across node restarts and partitions.
- Test remote control-plane behavior under partition, reconnect, and shutdown.

## Suggested Order

- [x] Finish the single-node control plane baseline (priority system-message delivery plus suspend/resume semantics).
- [x] Finish the remaining single-node control-plane features (`GetState`, `ReplaceState`, tracing control, code change, and unified external shutdown entrypoints).
- [x] Introduce actor-selected single-node receive primitives (`receive_next`, selective receive, and watermark-based reply matching).
- Add receive-timeout helpers and scheduler-yield guidance for longer selective-receive loops.
- Harden local ids/refs and introduce a future-proof local process-address type.

- Add node identity and connection management.
- Add remote send/reply/monitor/link basics.
- Add distributed control-plane authorization and forwarding.
- Add distributed naming only after transport and partition semantics are
  stable.
