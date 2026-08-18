# bperf reference

## Minimal benchmark shape

```ts
import {
  defineBrowserBenchmark,
  exact,
  fixture,
} from "bperf/browser";

const input = fixture("./fixtures/input.bin");

export default defineBrowserBenchmark({
  id: "library/subject",
  cases: [
    {
      id: "representative-input",
      async setup() {
        const response = await fetch(input.url);
        return new Uint8Array(await response.arrayBuffer());
      },
      measure(bytes) {
        return subject(bytes);
      },
      expect: exact(expectedResult),
    },
  ],
});
```

Keep acquisition in `measure()` when network fetching or streaming is part of
the subject. Put it in `setup()` when only the processing code should be
measured. `settle()` runs after CPU capture and before the live heap snapshot
when asynchronous work must finish first. Authors do not choose iteration or
sample counts.

## Common commands

```text
bperf doctor --engine all
bperf run <benchmark.ts> --budget 5m --message <text>
bperf history <benchmark-id> --format agent-context
bperf show <cycle-id> --diff
bperf show <cycle-id> --benchmark <benchmark-id>
bperf confirm <benchmark.ts> <cycle-id> --budget 5m
bperf accept <cycle-id>
bperf baseline show <benchmark-id> --json
```

`run` records a source-history cycle even when the candidate is negative,
equivalent, or inconclusive. History is independent of Git and preserves the
measured sequence of working-tree changes.

`show` and `accept` default to the latest cycle across every measured
benchmark; `--benchmark <benchmark-id>` scopes them to one benchmark's
cycles. Their text output names the selected benchmark, and an unscoped
`latest` in a multi-benchmark store prints a stderr notice.

## Reading evidence

Evaluate each benchmark case, browser engine, and primary metric separately:

- `workload.wall_ms` measures normalized subject wall time;
- `browser.cpu_profile.active_ms` comes from the native CPU profile;
- `browser.js_heap.live_bytes` describes the settled live JavaScript heap.

The CPU profile and Speedscope flamegraph share a representative trial. The
heap snapshot may come from another trial because it represents another metric
distribution. Absolute values from different engines are not interchangeable.

A faster candidate that fails correctness is a failure. A cross-engine result
is acceptable only when the configured policy accepts every engine-specific
classification and protected metric.

Present effect percentages with their baseline and candidate values, such as
`+52.45% (100ms -> 47.55ms)`. The values are workload-weighted geometric point
values, so they remain consistent with the reported effect across one or
several benchmark cases.

The default `run` and `confirm` output is a decision summary. Use
`bperf show <cycle-id> --json` for warnings, disputed engines,
threshold-sensitive intervals, guardrail failures, or sampling context; open
retained profiles only when they can explain the result or choose the next
hypothesis. Query the specific fields needed instead of printing a complete
JSON document. `.bperf/bperf.sqlite3` is internal canonical state, not a direct
inspection interface.

`bperf show <cycle-id>` lists retained artifact paths grouped by engine, and
`show --json` carries the same descriptors in `artifacts` (kind, engine,
capture scope, path). Cycles recorded by older bperf versions may omit the
field. `run`, `confirm`, and `show` JSON also carry `promotion_readiness`
(`ready`, `confirmation_required`, `searched_candidates`, `search_threshold`)
for branching between `accept` and `confirm` without provoking a refusal.

## Accepted-change commit template

Use a conventional performance subject:

```text
perf(compiler): avoid heap allocation in dead-code pass
perf(map): optimize integer-key hash lookup
perf(sync): reduce worker-pool lock contention
perf: intern repeated map strings with unique.Make
```

Structure the complete message as follows. Substitute values from the accepted
candidate, or from its confirmation when confirmation was required. Wrap
explanatory prose at 72 columns. Keep each metric's label, percentage, and
`(baseline -> candidate)` values on one physical line; if necessary, let that
line exceed 72 columns rather than splitting the metric. Keep a blank line
between engine blocks so `git log` remains easy to scan.

```text
perf(cea-608): use the standard 32-column screen

CEA-608 defines a 32-column caption grid. Reusing that standard size
instead of allocating 100 columns reduces construction, comparison,
copy, and scan work for every caption screen.

Chromium:
  CPU improvement: +53.28% (<before> -> <after>)
  Live heap improvement: +17.27% (<before> -> <after>)
  Wall-time improvement: +52.45% (<before> -> <after>)
  Anchor drift: +0.51%

Firefox:
  CPU improvement: +60.83% (<before> -> <after>)
  Live heap improvement: +34.06% (<before> -> <after>)
  Wall-time improvement: +60.58% (<before> -> <after>)
  Anchor drift: +0.00%

WebKit:
  CPU improvement: +46.10% (<before> -> <after>)
  Live heap improvement: +33.09% (<before> -> <after>)
  Wall-time improvement: +45.63% (<before> -> <after>)
  Anchor drift: -1.36%

Tests: <focused correctness tests>

Bperf-Benchmark: <benchmark-id>
Bperf-Cycle: <cycle-id>
```

Use the units printed by bperf (`ns`, `us`, `ms`, `s`, `b`, or `kb`). If an
accepted tradeoff includes a regression or equivalent metric, name that
classification instead of placing it under an improvement label.

## Recovery

Repeat the identical `run` or `confirm` command after interruption. bperf keeps
valid trials and retries only missing or invalid evidence. Investigate
environment identity or runtime-anchor drift before retrying an inconclusive
comparison; increasing the budget does not repair a changed environment.
