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

- [x] Make generation-bearing actor ids fully real, including safe slot reuse
  and stale-handle rejection.
- [x] Tighten `Ref` semantics so request refs, monitor refs, timer refs, and
  task refs have one obvious lifecycle and uniqueness story.
- [x] Introduce a process-address abstraction that still points only to local
  actors for now, but avoids baking "always local" assumptions into every API.
- [x] Keep the registry explicitly local and fast; do not mix in distributed
  naming semantics yet.

Why this matters:

- It lets the single-node runtime become more precise immediately.
- It reduces how much API churn is needed later if remote process addressing is
  added.

### 1.2 Make The Control Plane Real

Goal: turn the currently reserved system/control surface into an explicit,
runtime-owned local protocol for inspection, coordination, tracing, shutdown,
and code change.

- [x] Define the reserved system/control message set.
  - `Suspend`, `Resume`, `GetState`, `ReplaceState`, `TraceOn`, `TraceOff`,
    `Shutdown`, and `CodeChange` exist as reserved message kinds.
  - Runtime-originated traffic uses the mailbox reserve and bypass path instead
    of ordinary user admission rules.

- [x] Add a real system-message path.
  - Control messages are delivered through the reserved runtime lane.
  - User traffic cannot indefinitely starve suspend/resume/state/trace/code
    change operations.
  - Local and concurrent runtimes use the same priority story.

- [x] Implement suspend/resume semantics.
  - Suspending a process stops normal user-message execution without losing
    mailbox contents.
  - Exit and down signals still flow while suspended.

- [x] Implement process inspection and local state extraction.
  - `GetState` has a concrete reply shape and explicit unsupported/rejected
    failure modes.
  - Actor-scoped tracing can record receives, sends, mailbox depth, and
    scheduler assignment.

- [x] Implement controlled state replacement and code change.
  - `ReplaceState` validates payload shape and version.
  - `CodeChange` runs through the reserved runtime path.
  - State migration is versioned and can reject incompatible upgrades cleanly.

- [x] Add a real local upgrade workflow for `LocalRuntime`.
  - Supervisor-tree and application upgrades quiesce, run leaf-to-root code
    change, and resume the tree on success or failure.

- [x] Unify the shutdown story completely.
  - `shutdown_actor` and reserved `SystemMessage::Shutdown` now converge on the
    same control-plane shutdown path.
  - Raw actors stop with `ExitReason::Shutdown` by default, supervisors run
    coordinated subtree shutdown, and typed behaviors still see shutdown via
    their `info` path.

- [x] Decide and document partial-upgrade failure semantics.
  - v0 local upgrades are resume-only on failure: the runtime best-effort
    resumes the quiesced tree before returning an error.
  - Actors whose `CodeChange` already succeeded stay on the target version;
    partial progress is surfaced through `LocalUpgradeError::upgraded` and is
    not rolled back automatically.

- [ ] Decide whether v0 requires concurrent upgrade parity.
  - `ConcurrentRuntime` supports actor-level control operations, but it does not
    yet expose the same supervisor-tree/application upgrade workflow as
    `LocalRuntime`.

- [ ] Close the remaining control-plane test gaps.
  - Add focused coverage for shutdown under mailbox pressure.
  - Add focused coverage for code change while pending calls or selective
    receive loops are active.
  - Add focused coverage for supervisor behavior around suspended children and
    tracing overhead.

Why this matters:

- It improves debuggability and operability even if Lamport never becomes
  distributed.
- It creates the local semantics that a future distributed control plane would
  need to forward or authorize.

### 1.3 Add Real Process-Side Receive Semantics

Goal: let an actor decide what it is waiting for, scan its mailbox selectively,
and block until a matching message or timeout arrives, instead of always being
handed the next envelope by the runtime.

- [x] Introduce a receive-oriented runtime API.
  - Actors can `receive_next`, selectively receive, selectively receive after a
    watermark, and use runtime-managed receive timeouts.
  - The API is usable from both raw actors and higher-level behaviors.

- [x] Move mailbox choice closer to the actor.
  - The low-level actor contract now lets an actor choose its next envelope via
    `select_envelope`.

- [x] Define unmatched-message behavior.
  - Selective receive preserves unmatched relative order.
  - Watermark-based receive ignores older messages even if they match later.

- [x] Specify interaction with system traffic.
  - System/control, exit, and down traffic bypasses user selective receive so
    those signals remain observable.

- [x] Reconcile receive semantics with typed behaviors.
  - Typed behavior adapters coexist with the same mailbox and runtime envelope
    model.
  - Pending-call reply correlation is expressible via reply refs plus mailbox
    watermarks.

- [x] Revisit scheduler and mailbox invariants enough for v0.
  - Per-actor execution remains single-process-serialized.
  - The API documents yielding in repeated scan/wait loops so concurrent
    schedulers can rotate fairly.

- [x] Add core receive tests.
  - Watermark-based selective receive, timeout races, late replies, and
    exit/down delivery while waiting are covered.

- [ ] Add heavier fairness/performance characterization if mailbox scanning
  under large queues becomes a practical issue.

Why this matters:

- This is the biggest semantic step toward OTP-style process behavior even in a
  purely local runtime.
- It should be proven locally before adding network-induced races and failure
  modes.

### 1.4 Nail Down Runtime-Originated Delivery And Finalization

Goal: make runtime-owned traffic and actor teardown semantics explicit so local
failure handling stays predictable under mailbox pressure and shutdown races.

- [x] Make mailbox reserve semantics explicit for runtime-originated traffic.
  - The reserve applies to replies, task completions, call timeouts, exit
    signals, down notifications, timer events, and system messages.

- [x] Define runtime-originated overflow handling.
  - If runtime-owned delivery cannot be queued even after reserve handling, the
    target actor fails in one explicit way instead of dropping traffic.
  - Local and concurrent runtimes follow the same rule.

- [x] Tighten actor finalization guarantees.
  - Exit runs the terminate hook, cancels timers and call timeouts, detaches
    links, notifies monitors, cleans registry and supervisor-child bookkeeping,
    and retains a completed snapshot.
  - Dead snapshots remain queryable by actor id.

- [x] Add core teardown and overflow tests.
  - Runtime-originated overflow, cleanup after crash/shutdown, and
    completed-snapshot lookup after death are covered.

- [ ] Add a few more adversarial tests around managed shutdown plus mailbox
  pressure to finish the edge-case matrix.

Why this matters:

- Runtime-owned delivery and teardown behavior are core single-node semantics,
  not distributed follow-ons.
- Later multi-node work depends on having one precise local story for failure,
  overflow, and cleanup first.

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
- [x] Add receive-timeout helpers and scheduler-yield guidance for longer selective-receive loops.
- [x] Harden local ids/refs and introduce a future-proof local process-address type.
- [x] Add a real local upgrade workflow across supervisors and applications.
- [x] Nail down runtime-originated delivery overflow semantics and actor finalization guarantees.

- Add node identity and connection management.
- Add remote send/reply/monitor/link basics.
- Add distributed control-plane authorization and forwarding.
- Add distributed naming only after transport and partition semantics are
  stable.
