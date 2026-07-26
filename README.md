# bperf

`bperf` measures browser code in Chromium, Firefox, and WebKit. A benchmark is
one TypeScript module. bperf owns the browser lifecycle, fixture hosting,
repetition, correctness checks, native CPU profiles, flamegraphs, and
JavaScript heap captures.

I built bperf for a specific kind of performance work: change browser code, run
the same workload in each engine, and decide whether the change helped without
treating a faster incorrect result as an improvement.

<p align="center">
  <picture>
    <source
      media="(prefers-color-scheme: dark)"
      srcset="assets/bperf-logo-pixel-transparent-dark.png"
    >
    <source
      media="(prefers-color-scheme: light)"
      srcset="assets/bperf-logo-pixel-transparent.png"
    >
    <img
      src="assets/bperf-logo-pixel-transparent.png"
      alt="bperf pixel-art stopwatch racing toward a checkered flag"
      width="600"
    >
  </picture>
</p>

## The browser contract

Chromium, Firefox, and WebKit are one contract:

- Every requested engine must produce wall timing, a native CPU profile, a
  Speedscope flamegraph, and a native JavaScript heap snapshot.
- A missing capture capability fails before measurement. bperf does not
  silently drop an engine or artifact.
- Correctness is checked independently in every engine.
- Results stay indexed by benchmark case, engine, and metric. Raw measurements
  are not averaged across browsers.
- Historical comparisons check a fresh runtime anchor in every engine before
  trusting the old baseline.

`webkit` means Playwright's pinned, patched WebKit build. It does not automate
the Safari application installed by macOS.

This is not a general page-load tester. It is for deterministic browser
workloads whose result can prove that the same work still happened.

bperf is pre-1.0. Measurement and protocol files are versioned, but the CLI and
TypeScript authoring API may still change.

## Requirements

- Rust with edition 2024 support
- Node.js 24.12 or newer
- The Chromium, Firefox, and WebKit builds pinned by the sidecar's Playwright
  version

From a source checkout:

```text
npm --prefix sidecar ci
npm --prefix sidecar exec -- playwright install chromium firefox webkit
cargo build --locked
cargo run -- doctor --engine all
```

On Linux, Playwright may also require operating-system packages. Its
`install --with-deps` command can install both the browser builds and those
packages.

`doctor --engine all` is the capability gate. Run it on a new machine or after
changing Playwright or the installed browsers. Every doctor launches its browser
directly from Rust; the Node runtime is not part of browser capture.

To create and install an optimized build for the current platform:

```text
node scripts/package-release.ts --install
bperf --version
```

The packaging script builds the locked Cargo release and installs the
versioned TypeScript sidecar under Cargo's user `bin` directory. Node bundles
and hosts TypeScript benchmarks but does not launch browsers. Rust reads the
sidecar's pinned Playwright registry and directly launches and captures
Chromium, Firefox, and WebKit. The sidecar runs directly in Node; there is no
transpiled JavaScript tree. The bundle also contains the documentation,
examples, license, and `bperf-agent-loop` skill.

## Write a benchmark

```ts
import {
  defineBrowserBenchmark,
  exact,
  fixture,
} from "bperf/browser";

import { parseFragmentStream } from "./fragment-parser.ts";

const fragment = fixture("./fixtures/segment.mp4", {
  response: {
    contentType: "video/mp4",
    stream: { chunkSize: 64 * 1024 },
  },
});

export default defineBrowserBenchmark({
  id: "hls-mp4-parser",

  cases: [
    {
      id: "representative-fragment",

      async measure() {
        const response = await fetch(fragment.url);
        if (!response.body) {
          throw new Error("fragment response has no body");
        }
        return parseFragmentStream(response.body);
      },

      expect: exact({
        boxes: ["ftyp", "moof", "mdat"],
        samples: 184,
        duration: 6.006,
      }),
    },
  ],
});
```

The benchmark imports project code normally. `fixture()` turns a local file or
remote source into a pinned, browser-reachable loopback URL. The subject still
decides whether to use Fetch, XHR, streams, workers, or another browser API.

Put acquisition inside `measure()` when fetching or streaming is part of the
behavior being measured. Move it to `setup()` when only the processing work
belongs inside the measurement:

```ts
{
  id: "preloaded-fragment",

  async setup() {
    const response = await fetch(fragment.url);
    return response.arrayBuffer();
  },

  measure(bytes) {
    return parseFragment(bytes);
  },

  expect: exact(expected),
}
```

Benchmark authors do not choose iteration or sample counts. bperf calibrates
those independently for every case and engine, then records the decision in
the measurement set.

The complete API, fixture behavior, lifecycle, and current limitations are in
[Writing a benchmark](docs/AUTHORING.md). A verified integration with a
CommonJS hls.js checkout is in
[Benchmarking hls.js](docs/HLS_JS.md).

## Run the optimization loop

The example benchmark can be run directly from this checkout:

```text
cargo run -- run examples/managed/fragment-parser.bench.ts --budget 5m --message "Establish parser baseline"
```

The first run has no comparison, so it reports `measured` and prints a cycle
ID. Promote it explicitly:

```text
cargo run -- accept <cycle-id>
```

After changing the subject, run the same benchmark again:

```text
cargo run -- run examples/managed/fragment-parser.bench.ts --budget 5m --message "Reuse parsed box metadata"
cargo run -- show <cycle-id> --diff
```

With a promoted baseline, `run` reports one of four outcomes:

| Outcome | Meaning | Exit code |
|---|---|---:|
| `positive` | Every required engine satisfies the improvement policy. | 0 |
| `equivalent` | No protected regression was found, but the result is not a strict improvement. | 0 |
| `negative` | Correctness failed or a required engine regressed. | 1 |
| `inconclusive` | The evidence or runtime comparison cannot support a decision. | 2 |

A baseline-free `measured` run also exits 0. That only means the measurement
completed; it is not an improvement verdict.

After five candidates have been compared with one baseline, `accept` asks for
an independent confirmation:

```text
cargo run -- confirm <cycle-id> examples/managed/fragment-parser.bench.ts
cargo run -- accept <cycle-id>
```

Do not edit the source between the candidate and its confirmation.

The full baseline, candidate, confirmation, recovery, and evidence-reading
workflow is in [Running an optimization](docs/OPTIMIZATION.md).

The release bundle also contains `skills/bperf-agent-loop`. Install that
directory in an agent's personal skills location to give it the same baseline,
candidate, confirmation, and promotion workflow.

## What bperf records

Each completed `run` writes:

- an immutable measurement set under `.bperf/measurements/`;
- complete timing, CPU, flamegraph, heap, and correctness evidence for each
  case and engine;
- the adaptive pilot and final-sample decision in `sampling.json`;
- one representative CPU profile, flamegraph, and heap snapshot per case and
  engine;
- a content-addressed checkpoint of the project module graph that defined the
  measured browser bundle;
- an append-only optimization-cycle event, including negative, equivalent,
  inconclusive, and reverted changes;
- a comparison report when a promoted baseline exists.

An interrupted `run` or `confirm` resumes the compatible immutable schedule.
Valid trials remain valid; only missing or invalid attempts are retried.

The design and the reasons behind these boundaries are in
[bperf design](docs/DESIGN.md). Individual decisions are indexed in
[docs/adr](docs/adr/README.md).

## Advanced integration

The original YAML benchmark, variant descriptor, JSONL operation stream, local
adapter, and verifier process remain available for subjects that cannot use the
TypeScript authoring path:

```text
cargo run -- validate examples/browser-benchmark.yaml --variant examples/browser-variant-baseline.yaml
cargo run -- measure examples/browser-benchmark.yaml examples/browser-variant-baseline.yaml --final-samples 20
```

A fixed count of `N` means `N` complete final trials for each benchmark case
and engine. Each trial contains wall timing, CPU, flamegraph, and heap evidence
from the same workload execution.

Most benchmark authors should use `bperf run <benchmark.ts>`.

## Development

See [CONTRIBUTING.md](CONTRIBUTING.md) for the fast test suite, real-browser
release gates, design expectations, and documentation style.

## License

bperf is available under the [MIT License](LICENSE).
