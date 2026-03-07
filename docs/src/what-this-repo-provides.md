# What This Repo Provides

`lamport` is a Rust crate for building OTP-style systems on a single node. It
is not a BEAM clone and it does not implement distributed Erlang semantics. The
focus here is the runtime model that makes OTP-style applications useful:
isolated actors, explicit mailboxes, failure propagation, restart policies, and
observable runtime state.

## Core Runtime Profiles

The crate exposes two execution profiles with the same actor model:

- `LocalRuntime` runs actors in a single thread. It is the simplest option for
  deterministic tests, simulations, and examples where you want explicit
  control over progress with `run_until_idle` or `block_on_next`.
- `ConcurrentRuntime` runs actors across multiple schedulers backed by Tokio.
  Use it when many actors need to make progress at the same time or when you
  want dirty pools for blocking work.

Both runtimes expose similar top-level operations:

- spawn raw actors, `GenServer`s, `GenStatem`s, and supervisors
- send typed user messages or full envelopes
- inspect actors, supervisors, and runtime-wide metrics
- resolve registered names through the registry

## Actor Model

At the bottom is the `Actor` trait. An actor processes one envelope at a time
and returns an `ActorTurn` telling the runtime whether to continue, yield, or
stop.

This model gives you:

- generation-bearing `ActorId`s with stale-handle rejection on slot reuse
- local `ProcessAddr` values for routing without baking "always local" into
  every API
- bounded mailboxes
- actor-selected receive with selective receive, mailbox watermarks, and
  receive-timeout helpers
- request/reply with reply tokens and optional timeouts
- timers keyed by `TimerToken`
- links, monitors, and trap-exit semantics
- explicit exit reasons and lifecycle events

The turn contract is intentionally synchronous. If you need slow I/O or CPU
work, dispatch it through `Context::spawn_blocking_io` or
`Context::spawn_blocking_cpu` and handle completion as a later runtime info
message. If you keep an actor in a selective-receive wait loop for many turns,
call `Context::yield_now()` after making progress so concurrent schedulers can
rotate fairly.

## Higher-Level Behaviours

If you want more OTP-shaped structure without manually downcasting every
message, the repo provides typed behaviour layers:

- `GenServer` for request/reply servers, background workers, and stateful
  services with `handle_call`, `handle_cast`, and `handle_info`
- `GenStatem` for explicit finite-state workflows where the current state is a
  first-class concept
- `Supervisor` for owning child specs, restart strategies, shutdown policy, and
  restart-intensity windows
- `Application` for booting a root supervisor as the public entry point to a
  runtime-owned tree

These behaviours are adapters over the same actor runtime, not a separate
execution model.

## Operational Features

The repo also provides the pieces needed to operate and inspect a runtime:

- actor and supervisor snapshots
- actor tree reconstruction
- retained lifecycle events and crash reports
- incremental event consumption with `EventCursor`
- Prometheus text export through `export_metrics_prometheus`
- registered names via the runtime registry, resolved as local process
  addresses

The [`examples/observability.rs`](../../examples/observability.rs) example is
the best compact tour of these APIs.

## Example Coverage

The repository examples map closely to the main building blocks:

- [`examples/chat.rs`](../../examples/chat.rs): raw actors plus a typed
  `GenServer`, timers, request/reply, and room-style fanout
- [`examples/statem.rs`](../../examples/statem.rs): `GenStatem` for an
  explicit state machine
- [`examples/supervision.rs`](../../examples/supervision.rs): supervisor
  flags, child specs, restarts, monitors, and application boot
- [`examples/observability.rs`](../../examples/observability.rs): snapshots,
  events, crash reports, and metrics
- [`examples/runtime_comparison.rs`](../../examples/runtime_comparison.rs):
  when `ConcurrentRuntime` helps over `LocalRuntime`
- [`examples/multi_llm_agents.rs`](../../examples/multi_llm_agents.rs):
  blocking I/O tasks wrapped inside OTP-style supervision

## Current Boundaries

The current repo is deliberately scoped. It does not yet provide:

- distributed node-to-node messaging
- hot code upgrade workflows
- priority mailboxes
- durable or journaled mailboxes

That makes the crate a good fit for single-process OTP-style services, control
planes, simulations, orchestration flows, and actor-based prototypes in Rust.
