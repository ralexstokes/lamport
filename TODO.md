## Multi-Node Regime

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

- Add node identity and connection management.
- Add remote send/reply/monitor/link basics.
- Add distributed control-plane authorization and forwarding.
- Add distributed naming only after transport and partition semantics are
  stable.
