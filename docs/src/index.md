# lamport Docs

`lamport` is an OTP-inspired, single-node actor runtime scaffold for Rust.
The repository gives you both low-level actor primitives and higher-level
behaviours for building OTP-style services:

- raw turn-based actors
- typed `GenServer` and `GenStatem` adapters
- supervisors, child specs, and restart policies
- application boot helpers for root supervisor trees
- runtime introspection, lifecycle events, crash reports, and Prometheus-style
  metrics
- both a deterministic local runtime and a concurrent multi-scheduler runtime

Use this book in two layers:

- [`What This Repo Provides`](./what-this-repo-provides.md) explains the crate's
  main building blocks and where each one fits.
- [`Common Use Cases`](./common-use-cases.md) shows which APIs and examples to
  reach for in day-to-day usage.
- [`SPEC.md`](./SPEC.md) is the normative functional specification. It is
  intentionally language-neutral and focused on runtime semantics rather than
  repository layout.
