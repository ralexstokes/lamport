# Common Use Cases

This page is a practical guide to choosing the right `lamport` APIs for common
tasks. For the precise runtime rules, see the [spec](./SPEC.md).

## 1. Start with a Small, Deterministic Runtime

If you are building tests, simulations, or a small in-process service, start
with `LocalRuntime`.

Why:

- actor progress is explicit
- test control is simple with `run_until_idle`
- timers and asynchronous completions can be driven with `block_on_next`

Relevant APIs:

- `LocalRuntime::spawn`, `spawn_gen_server`, `spawn_gen_statem`,
  `spawn_supervisor`
- `LocalRuntime::send`
- `LocalRuntime::run_until_idle`
- `LocalRuntime::block_on_next`

See:

- [`examples/chat.rs`](../../examples/chat.rs)
- [`examples/statem.rs`](../../examples/statem.rs)
- [`tests/application_boot.rs`](../../tests/application_boot.rs)

## 2. Use a Raw Actor for Protocol Adapters or Simple Turn Logic

Use the `Actor` trait when:

- you want to match directly on `Envelope`
- you need full control over replies, timers, and system messages
- the protocol is small enough that explicit payload downcasting is acceptable

This is a good fit for thin adapters, observers, probes, and actors that mostly
react to runtime events.

See:

- `ChatClient` in [`examples/chat.rs`](../../examples/chat.rs)
- `MonitorActor` in [`examples/observability.rs`](../../examples/observability.rs)

## 3. Use `GenServer` for Stateful Request/Reply Services

Reach for `GenServer` when you want a typed stateful service with a familiar
OTP split between synchronous calls, asynchronous casts, and informational
messages.

This is the default choice for:

- registries and routers
- session or room managers
- workers that maintain mutable service state
- actors that need delayed replies through `CallOutcome::NoReply`

The [`examples/chat.rs`](../../examples/chat.rs) room process is a concrete
pattern: callers use `ctx.ask(...)`, the server replies with typed values, and
timers or runtime notifications arrive through `handle_info`.

## 4. Use `GenStatem` When State Transitions Are the Design

Use `GenStatem` when the current state is a first-class part of the problem:

- protocol handshakes
- workflow engines
- device or lifecycle controllers
- state machines that need both mutable data and explicit transitions

The [`examples/statem.rs`](../../examples/statem.rs) traffic light example
shows the intended shape: state identifiers stay small and explicit, while
longer-lived counters and timers live in the separate data struct.

## 5. Put Failure Policy in Supervisors

Use `Supervisor` when child lifetime and restart policy matter more than the
child's business logic.

This gives you:

- ordered child specs
- `OneForOne`, `OneForAll`, and `RestForOne` restart scope
- per-child `Restart` and `Shutdown` policy
- restart-intensity limits through `SupervisorFlags`

Use `Application` on top when you want a clean boot entry point for a root
supervisor tree.

See:

- [`examples/supervision.rs`](../../examples/supervision.rs)
- [`tests/application_boot.rs`](../../tests/application_boot.rs)

## 6. Offload Slow Work Instead of Blocking Actor Turns

Actor turns are synchronous. If a turn needs to perform blocking I/O or
CPU-heavy work, send it to a dirty pool:

- `Context::spawn_blocking_io` for network calls, filesystem work, and other
  blocking I/O
- `Context::spawn_blocking_cpu` for CPU-bound jobs

When the job completes, the runtime routes the result back as a task completion
message. This keeps the actor model intact without stalling normal schedulers.

See:

- `ProviderServer` in
  [`examples/multi_llm_agents.rs`](../../examples/multi_llm_agents.rs)

## 7. Choose `ConcurrentRuntime` for Parallel Actor Workloads

Move to `ConcurrentRuntime` when many independent actors need real parallel
progress. This is most useful when:

- actor turns are still synchronous, but expensive
- many actors are runnable at once
- you want dirty pools available by default

The [`examples/runtime_comparison.rs`](../../examples/runtime_comparison.rs)
program is the clearest demonstration of the tradeoff. It compares the same
workload on `LocalRuntime` and `ConcurrentRuntime`.

## 8. Treat Observability as a First-Class API

The repo already exposes runtime state in a form suitable for tests, dashboards,
and debugging tools. Reach for these APIs when you need operational visibility:

- `actor_snapshot` and `supervisor_snapshot`
- `actor_tree` and `introspection`
- `lifecycle_events`, `event_log`, and `events_since`
- `crash_reports`
- `export_metrics_prometheus`

See:

- [`examples/observability.rs`](../../examples/observability.rs)

## 9. Run the Existing Examples First

The fastest way to learn the crate is to run the example closest to your use
case:

```sh
cargo run --example chat
cargo run --example statem
cargo run --example supervision
cargo run --example observability
cargo run --example runtime_comparison
```

The multi-provider example also needs API keys:

```sh
OPENAI_API_KEY=... ANTHROPIC_API_KEY=... cargo run --example multi_llm_agents
```
