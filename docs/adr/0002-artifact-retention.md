# ADR 0002: Golden capture contracts and representative retention

Status: accepted · 2026-07-25

## Context

Every sample needs a native CPU profile and heap snapshot to produce its scalar
metrics. Keeping every raw capture indefinitely makes measurement storage grow
with the statistical sample count, even though most captures are
interchangeable diagnostics.

Preflight probes and frozen workload inputs are similarly useful while a
measurement is running or resumable, but the durable environment record and
trial log replace them after completion.

Deleting artifacts as trials finish would keep storage bounded, but bperf would
not yet know which observation best represents the completed distribution.
Choosing a fixed sample index has the same problem and behaves poorly when a
trial is retried.

Capture adapters also depend on three different browser-native formats. Live
browser tests prove end-to-end support, but they are too slow and
environment-dependent to be the only regression coverage for parsers and
Speedscope normalization.

## Decision

Every trial captures and validates a native CPU profile, Speedscope flamegraph,
and JavaScript heap snapshot. Once the active schedule is complete, bperf
selects artifacts independently for each benchmark case and engine:

- the CPU profile and its flamegraph come from the final trial nearest the
  median `browser.cpu_profile.active_ms`;
- the heap snapshot comes from the final trial nearest the median
  `browser.js_heap.live_bytes`;
- pilot or warmup trials are used only when a measurement has no final trials;
- ties are resolved by stable trial identifier.

The immutable `artifact-retention.json` records the selected trial, metric,
median, observed value, artifact descriptor, and aggregate retained/discarded
counts. It is written before unselected files are removed, making cleanup
resumable. All trial records retain the original paths, sizes, formats, and
SHA-256 digests. Reopening a measurement validates the manifest against those
records and revalidates every retained file.

Once the retention manifest and measurement summary are durable, bperf removes
the raw preflight captures and measurement-local frozen workloads. Cleanup is
idempotent and refuses incomplete measurements. A failed or interrupted
measurement keeps both directories so it can resume without reconstructing
execution state.

Checked-in golden captures cover Chromium V8, Firefox Gecko Profiler, and
WebKit ScriptProfiler input together with exact Speedscope output. The
TypeScript tests run the production format adapters over those fixtures. Rust
tests independently enforce the complete artifact set, path containment, size,
and digest contract. The Firefox adapter reads only the stable core-dump
framing and each protobuf node's identifier and shallow size; names, edges, and
allocation stacks remain opaque. Its compact format fixture locks down scalar
extraction, while the mandatory real-browser contract test covers Firefox's
live production output.

## Consequences

- Statistical evidence still comes from every completed trial.
- Persistent raw-artifact storage is bounded by case and engine count
  rather than sample count.
- CPU and heap representatives may come from different trials because they
  represent different distributions.
- Every heap representative is the artifact from which its live-byte scalar was
  derived.
- Trial evidence is append-only; finalization changes only which diagnostic
  payload files and resumable execution inputs remain present.
- Completed measurement sets retain the compact environment, trial, fixture,
  and representative-profile evidence needed for history, comparison, and
  promotion.
- Format-normalizer regressions fail the fast suite without launching browsers,
  while real Chromium, Firefox, and WebKit remain the release gate.
