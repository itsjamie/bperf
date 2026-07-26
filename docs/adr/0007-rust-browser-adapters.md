# ADR 0007: Rust owns browser capture through knowledge-oriented crates

Status: accepted · 2026-07-26

## Context

The original capture sidecar owned Playwright browser objects and separate
TypeScript implementations for Chromium, Firefox, and WebKit. That divided
measurement ownership between Rust and Node, exposed Playwright-private objects
to the capture path, and gave retained browser lanes two lifecycle models.

After moving capture to Rust, 23 private modules still lived in the binary
crate. Browser capture, measurement persistence, evidence decisions, and
installed-runtime discovery had no explicit public Interfaces. File-level
cycles made those knowledge boundaries difficult to see and encouraged
orchestration code to reach into persistence paths and installation layout.

Three designs were considered:

1. Keep every browser adapter in Node.
2. Keep automation in Node and proxy native protocol messages or capture
   payloads to Rust.
3. Launch the pinned browsers from Rust and keep each engine's automation,
   profiling, heap capture, and recovery in one Rust adapter.

For the Rust layout, three alternatives were considered:

1. Keep one binary crate and rely on private file modules.
2. Create a crate for each engine or each source file.
3. Create a small number of deep crates around installed-runtime discovery,
   browser capture, measurement-set lifecycle, and evidence decisions.

Proxying adds another transport without hiding either side's protocol knowledge
or giving one runtime responsibility for failure recovery. Direct ownership
gives the measurement core one lifecycle while keeping engine-specific
complexity behind private adapters.

One-file and one-engine crates would add Interfaces without hiding meaningful
complexity. They would also distribute the all-engine contract across packages.
Knowledge-oriented crates preserve Locality and establish a one-way dependency
graph.

## Decision

`BrowserLab` routes Chromium, Firefox, and WebKit to retained Rust adapters.
There is no production fallback to Node.

The Cargo workspace has four library crates beneath the `bperf` application:

- `bperf-runtime` exposes `installation` and hides release layout, environment
  lookup, Playwright registry parsing, cache conventions, and Node path
  normalization.
- `bperf-browser` exposes `lab` and `artifacts`. Engine protocols, process
  containment, workload injection, and native parsers remain private.
- `bperf-measurement` exposes `manifest`, `schedule`, `sampling`, `store`, and
  `retention`.
- `bperf-decision` exposes `environment`, `comparison`, `baseline`, and
  `lineage`.

Dependencies point from the application to those crates, from
`bperf-decision` to `bperf-measurement` and `bperf-browser`, from
`bperf-measurement` to `bperf-browser`, and from `bperf-browser` to
`bperf-runtime`. No library depends on the application or on a higher layer.
There is no shared types crate.

- Chromium owns its remote-debugging pipe, CDP sessions, V8 profiling, and V8
  heap snapshots.
- Firefox owns its Juggler pipe, RDP actors, Gecko Profiler capture, and
  `.fxsnapshot` parsing.
- WebKit owns its private inspector pipe, page-proxy and target sessions,
  ScriptProfiler capture, and WebKit heap snapshots.

The shared browser-process module owns descriptor pipes and process-tree
containment. Unix launches each browser in a process group; Windows assigns the
suspended root to a kill-on-close Job Object before resuming it. Shutdown does
not succeed until the process group is absent or Job Object accounting reports
zero active processes.

One browser process is retained per engine and measurement set. Each trial gets
a fresh context and page. A protocol or capture failure terminates the lane; a
later attempt launches a new process.

The runtime-installation Module owns registry discovery and pinned revision
selection. The browser-workload Module owns the versioned in-page workload and
local-only WebSocket policy. The browser artifacts Module owns the complete
three-file artifact set, immutable file identity, validation, and the common
Speedscope document shape. Engine protocols, native formats, and sample
selection remain private to their adapters.

`BrowserLabConfig` is removed. A validated `RuntimeInstallation` crosses the
Seam once, and adapters request pinned browser identities from it. Measurement
orchestration uses domain operations such as `freeze_workload` and
`write_environment_record`; it does not call a public generic file writer.

Node remains the TypeScript benchmark bundler, fixture resolver, and loopback
benchmark host. It does not launch browsers or capture evidence. The former
TypeScript browser adapters and their capture transport are deleted.

Capture protocol 12 and environment schema 5 identify the ownership change.
Benchmark-host readiness remains version 2, doctor output remains schema 2, and
measurement records do not change shape. Environments from Node-owned browser
adapters require remeasurement.

This decision supersedes ADR 0004's assumption that retained lanes live in the
sidecar and ADR 0002's assignment of native artifact normalization to
TypeScript. Their isolation and evidence-retention decisions remain in force.

## Validation

Fast tests cover protocol framing and routing, network cancellation, native
capture parsing, Speedscope goldens, artifact validation, process-containment
mechanics, and old-environment rejection.

The real-browser workflow proves for all three engines:

- direct launch without Node and complete CPU, flamegraph, and heap capture;
- a stable retained root PID across repeated captures;
- fresh context state and lane reopening after failure;
- managed discovery and complete measurement;
- shutdown with no active process left in the owned process group or Job
  Object.

Retirement does not require parity evidence on operating systems where the
former Node implementation was never release-qualified. A platform release
must run the same Rust live gates on that platform, but unverified historical
Node code is not retained as its oracle.

## Consequences

- Browser capture no longer starts Node for any engine.
- The former binary-only Modules now have explicit, reviewable public
  Interfaces and one-way crate dependencies.
- Engine adapters remain together, preserving one authoritative all-engine
  capture contract.
- The packaged Node runtime contains only the benchmark host, authoring module,
  project bundler, package manifests, and pinned Playwright registry.
- Playwright revision updates require native-format and live browser
  conformance evidence.
- CDP, Juggler, RDP, Gecko Profiler, Web Inspector, and Playwright-private
  objects never cross the `BrowserLab` adapter seam.
