# ADR 0016: Scope GPU benchmarks to the engines that implement GPU evidence

Status: accepted - 2026-10-18

## Context

Benchmark subjects that use WebGL or WebGPU include renderers, compute
kernels, resource upload and shader compilation paths, and libraries whose cost
is mostly JavaScript-side command issue. The motivating subjects ship on
set-top boxes and televisions, whose browsers are almost exclusively
Chromium-based.

These subjects fit bperf's deterministic, repeatable workload model, but they
cross several parts of the existing contract.

GPU execution is asynchronous. `measure()` resolving only proves that commands
were submitted. Wall time describes the subject only when the GPU work has
completed before the clock stops.

GPU availability depends on how each engine is launched. The Chromium lane
runs the headless shell with SwiftShader enabled, so WebGL currently executes
on the CPU through ANGLE's Vulkan backend and no WebGPU adapter is available.
In the pinned Linux builds, headless Firefox created no WebGL2 context and
neither Firefox nor WebKit exposed `navigator.gpu`.

The useful GPU evidence is engine-private and uneven. Chromium's DevTools
protocol exposes GPU-process trace events through `Tracing` and exact GPU
device and driver identity through `SystemInfo`. Firefox's Gecko Profiler can
sample rendering threads but exposes no GPU timing. WebKit's inspector can
record canvas API calls and compositing events, also without GPU timing, and
reports its renderer as a generic "Apple GPU". Page-visible GPU timers are
Chromium-only (`timestamp-query`) or disabled (`EXT_disjoint_timer_query`).

The GPU and driver are part of the measured environment, but host identity
records only the CPU, memory, and operating system. The runtime anchor measures
JavaScript CPU throughput and cannot detect GPU clock, thermal, or driver
changes between a historical baseline and a fresh run.

The managed TypeScript path always measures all three engines. The measurement
layer already accepts a subset through `browser.engines`, and comparisons and
verdicts are already keyed by case and engine.

Three designs were considered:

1. Keep every GPU capability on all three engines. Each GPU API and artifact
   waits until Firefox and WebKit can produce equivalent evidence.
2. Let a benchmark declare a narrower engine scope explicitly, and offer GPU
   evidence only to benchmarks whose scope is within the engines that
   implement it.
3. Make GPU benchmarks implicitly Chromium-only.

The first design blocks the motivating subjects on capabilities their target
devices do not use, and it reduces GPU evidence to what the weakest engine can
report. The third design is a shared abstraction that silently supports one
engine, which the browser contract forbids.

## Decision

### Engine scope

A managed benchmark may declare its engine scope:

```ts
export default defineBrowserBenchmark({
  id: "tile-renderer",
  engines: ["chromium"],
  gpu: "webgl",
  cases: [/* ... */],
});
```

Omitting `engines` selects Chromium, Firefox, and WebKit. A declared scope must
be nonempty and free of duplicates. The scope is part of benchmark identity,
so measurement sets with different scopes are never compared. Discovery,
preflight, runtime anchors, environment validation, comparison, and the verdict
fold cover exactly the declared engines. Reports name the scope so a
Chromium-only result is not mistaken for a three-engine one.

Every engine in scope still satisfies the complete common capture contract:
wall timing, native CPU profile, Speedscope flamegraph, and native JavaScript
heap snapshot.

### GPU requirement

A benchmark may declare one GPU API: `"webgl"`, then `"webgl2"`, then
`"webgpu"`, introduced in that order. An engine supports a GPU API only after
its adapter implements the GPU contract below and passes the real-browser test
for that API. Chromium is the only engine that will implement it initially.

Declaring `gpu` with an engine scope that includes an unsupported engine fails
before discovery and names the engine, the API, and the narrower scope that
would be accepted. bperf never drops the unsupported engine on its own.

Omitting `gpu` keeps the existing lanes, environment identity, anchors, and
artifacts unchanged, so current baselines remain comparable.

### GPU contract

The GPU declaration selects a GPU lane configuration. Launch flags that enable
hardware acceleration and the declared API are adapter-private and part of
adapter identity. Benchmarks with and without a GPU declaration never share a
lane.

Preflight creates a context for the declared API in a fresh page. A missing
API, a context that cannot be created, or a lost context fails preflight. A
context loss during a trial invalidates the attempt.

A software adapter, such as SwiftShader, llvmpipe, or WARP, satisfies the
requirement. Its use is recorded, never hidden. The environment identity gains
the declared API, the GPU devices and driver versions reported by the engine,
the adapter description the page sees, and a hardware or software
classification. Any difference in these fields changes the environment
fingerprint, so a software-rendered set is never compared with a
hardware-rendered baseline.

A GPU benchmark also records a versioned GPU anchor: a fixed fill and readback
workload for the declared API, observed 31 times alongside the JavaScript
anchor. The comparator applies the same rule as ADR 0001 to it: drifted,
inconclusive, or missing GPU anchor evidence makes performance inconclusive
for that engine.

A native GPU trace is deferred. The intended shape is a Chromium `Tracing`
capture of the GPU process, compositor, and graphics-backend categories around
the same workload execution, retained as a new artifact kind with a reported
GPU-process CPU metric. It needs its own artifact-retention and stability
evidence before it can join the trial contract, and the first phase is useful
without it: readback-fenced wall time and CPU profiles already expose
command-issue cost and completed GPU work.

GPU evidence is collected on a loopback page served by the adapter, because
WebGPU exists only in secure contexts and the probe's `about:blank` page is not
one. When no hardware WebGPU adapter is available, the probe requests the
browser's fallback adapter, which is recorded as software.

### Authoring rules

bperf does not insert fences. The result a GPU case returns comes from a
readback (`readPixels` for WebGL, buffer mapping for WebGPU), and that readback
is both the correctness evidence and the point where GPU work must be complete.
`bperf/browser` provides readback-digest helpers so that the correct pattern
is also the shortest one. A case that returns without reading back GPU output
measures command submission, and the authoring guide states that boundary
explicitly.

Correctness stays with `exact()`. Because software and hardware adapters may
rasterize differently, GPU cases must return results that are deterministic
across adapters: integer compute output, or rendering without antialiasing,
precision-dependent blending, or sub-pixel geometry.

Rendering uses `OffscreenCanvas` or an undisplayed canvas. Cases must not pace
work with `requestAnimationFrame`.

## Validation

Fast tests cover engine-scope declaration, description, and identity; rejection
of a GPU declaration whose scope includes an unsupported engine; scoped anchor
and environment validation; comparison refusal across different scopes; lane
configuration selection; GPU identity and fingerprint changes for hardware and
software adapters; GPU anchor validation; inconclusive comparison on GPU anchor
drift; and the Chromium trace parser against a checked-in golden capture.

`bperf doctor --engine chromium --gpu <api>` proves each GPU API. The ignored
real-browser test measures the WebGL example on a Chromium GPU lane and
requires valid, correct trials and a recorded adapter. The WebGL example
produces the same digest on hardware (Intel through ANGLE) and on SwiftShader,
and a batched rewrite of it was measured as a positive change with both anchors
stable. A case that loses its WebGL context mid-trial invalidates the attempt.

## Consequences

- WebGL and WebGPU subjects use the managed path without engine-specific
  benchmark code, on the engine their target devices run.
- A Chromium-scoped result says nothing about Firefox or WebKit, and the
  scope is visible wherever the result is reported.
- Adding GPU support to Firefox or WebKit later means implementing the same
  GPU contract in that adapter. Benchmarks can then widen their scope, which
  starts a new baseline because the scope is part of identity.
- Machines without a usable GPU still run GPU benchmarks on a software
  adapter, and their evidence remains separate from hardware evidence.
- Results come from the pinned desktop Chromium build and the host GPU. They
  indicate whether a change reduces work in Chromium; they do not reproduce a
  television's older Chromium, GPU, or frame budget.
- Benchmarks whose output is not deterministic across adapters cannot use the
  common path until a tolerance verifier is designed.
- GPU benchmarks add GPU identity and anchors to the environment record;
  records without them, and every non-GPU benchmark, are unchanged.
- GPU-process work stays outside the evidence until the deferred trace lands.
