# Writing a benchmark

A bperf benchmark describes the work, the inputs, the measurement boundary,
and the result that proves the work stayed correct. Browser launch, fixture
hosting, profiling, repetition, and sample sizing stay outside the benchmark.

The common path is deliberately small. If a benchmark needs a custom build
pipeline, opaque runtime imports, or a verifier that cannot be expressed as an
exact JSON result, use the [advanced adapter](#when-the-common-path-does-not-fit).

## Minimal benchmark

```ts
import {
  defineBrowserBenchmark,
  exact,
  fixture,
} from "bperf/browser";

import { decode } from "../src/decode.ts";

const input = fixture("./fixtures/input.bin");

export default defineBrowserBenchmark({
  id: "library-decoder",

  cases: [
    {
      id: "representative-input",

      async setup() {
        const response = await fetch(input.url);
        return new Uint8Array(await response.arrayBuffer());
      },

      measure(bytes) {
        return decode(bytes);
      },

      expect: exact({
        records: 42,
        checksum: 173504052,
      }),
    },
  ],
});
```

Benchmark and case IDs may contain letters, digits, dots, dashes, and
underscores. IDs are part of the persisted evidence, so use domain names that
will still make sense in history.

## Case lifecycle

Each case has one required `measure()` function and two optional lifecycle
functions:

```ts
{
  async setup() {
    return prepareState();
  },

  measure(state) {
    return runSubject(state);
  },

  async settle(state) {
    await state.backgroundWork;
  },

  expect: exact(expected),
}
```

`setup()` runs once on a fresh page before timing and CPU profiling begin. Its
return value is passed to every invocation of `measure()` in that trial.

`measure()` is one semantic operation. bperf may call it repeatedly as one
calibrated batch, but the benchmark does not need to know the batch size.
Repeated calls must return the same JSON value.

`settle()` runs after the CPU profiler stops and before the live heap capture.
Use it only when the measured operation starts work that must finish before the
heap represents a stable page. Work done in `settle()` is outside wall and CPU
metrics, but it can change the captured heap.

Every trial gets a new browser context and page. `setup()` therefore cannot
carry state between trials. The browser process is retained within an engine's
measurement lane so startup does not dominate short workloads.

## Dedicated workers and iframes

Code run by page-owned dedicated workers and same-origin or cross-origin
loopback iframes is part of the trial's browser evidence. Await measured child
work before `measure()` resolves so wall timing and CPU evidence describe the
complete semantic operation:

```ts
async measure() {
  const worker = new Worker(new URL("./parser-worker.ts", import.meta.url), {
    type: "module",
  });
  const result = await runWorker(worker);
  worker.terminate();
  return result;
}
```

If the measured operation leaves asynchronous child work that must affect the
heap, retain the relevant objects and await completion in `settle()`. Work done
by `settle()` remains outside wall and CPU metrics.

One trial may retain several native artifact groups called capture scopes.
Chromium and WebKit expose some child realms separately; Firefox captures the
browser context as one group. Capture scopes are diagnostic evidence, not
additional benchmark cases, and bperf aggregates their CPU and live-heap
scalars within each engine.

Shared workers and service workers are not part of the common capture contract.
Use the advanced protocol when a subject requires either lifecycle.

## Choose the measurement boundary

The most important authoring decision is what belongs in `measure()`.

This includes fetching and stream consumption:

```ts
async measure() {
  const response = await fetch(fragment.url);
  return parseStream(response.body);
}
```

This measures parsing after the bytes have been acquired:

```ts
async setup() {
  const response = await fetch(fragment.url);
  return response.arrayBuffer();
},

measure(bytes) {
  return parseBytes(bytes);
}
```

Both are valid benchmarks, but they are different subjects. Name the benchmark
and case so the boundary is obvious.

Input belongs to the benchmark, not to the candidate implementation. Allowing
each variant to select its own work can make a faster result mean that less
work happened.

## Correctness

The current TypeScript API supports exact JSON expectations:

```ts
expect: exact({
  byteLength: 3836611,
  tracks: [{ id: 1, sampleCount: 100 }],
})
```

`measure()` must return a JSON-compatible value containing only null, booleans,
finite numbers, strings, arrays, and plain objects. bperf rejects unsupported
values before treating a trial as valid.

Each case is invoked once on every engine during discovery and checked again in
every captured trial. Batched invocations must also agree with one another.
This catches stateful work that happens to pass on its first call.

The TypeScript path does not yet expose numeric tolerances, schemas, or a
custom verifier. The advanced protocol remains available when an exact result
would be misleading or impractical.

## Fixtures

`fixture()` returns a standard `URL`:

```ts
const fragment = fixture("./fixtures/segment.mp4");

await fetch(fragment.url);
subject.load(fragment.url);
new Request(fragment.url);
```

The fixture does not decide how browser code consumes the resource.

### Local files

A relative path is resolved from the benchmark module. bperf hashes the body
and serves it from a managed loopback origin. The body hash participates in
benchmark identity; the temporary port and URL do not.

### Remote sources

An HTTP or HTTPS URL is an acquisition source, not the URL used during trials.
On first discovery, bperf:

1. downloads the body outside measurement;
2. records the source URL and final redirected URL;
3. stores the body by SHA-256;
4. pins it in the benchmark's fixture lock;
5. serves the pinned body from loopback during trials.

The browser is not allowed to make unexpected non-loopback requests.

bperf reuses a pinned body when the same fixture descriptor appears again.
There is not yet a fixture-refresh command. Prefer immutable or versioned
remote URLs. If the content must change, change the fixture descriptor and
establish a new baseline.

### Response behavior

bperf infers common content types and supports byte-range requests by default.
The benchmark may override the content type and make delivery deterministic:

```ts
const media = fixture("./fixtures/segment.mp4", {
  response: {
    contentType: "video/mp4",
    stream: {
      chunkSize: 64 * 1024,
      intervalMs: 2,
    },
  },
});
```

`chunkSize` must be positive. `intervalMs` is optional and may be zero.
Response behavior participates in benchmark identity.

Custom response headers, status codes, and redirect plans are not part of the
current TypeScript fixture API.

## Project imports and variant identity

Import the subject as project code normally does:

```ts
import { parseInitSegment } from "../src/utils/mp4-tools.ts";
```

bperf creates an in-memory browser ESM bundle using the project's installed
packages and TypeScript configuration. It handles TypeScript syntax, ESM,
CommonJS dependencies, package export maps, and path aliases without changing
the project's package type.

The measured variant includes:

- the benchmark and every source file in the browser bundle;
- package manifests and lockfiles that affect dependency resolution;
- TypeScript or JavaScript configuration that affects emitted semantics.

Statically resolvable dynamic imports are part of the bundle graph. Runtime
ports, generated fixture URLs, and bperf's own sidecar source do not become
part of the project checkpoint.

The source graph is read twice before a cycle is recorded. If the graph changes
while bperf is attaching source evidence to a completed measurement, the run
fails instead of recording the wrong source state.

## Cases must be repeatable

Discovery, pilot sizing, pilot capture, and final trials all invoke the case.
A case must be safe to repeat in disposable browser state.

The common path does not yet define a lifecycle for:

- an externally single-shot operation;
- a workload that must preserve state across trials;
- leak growth measured over a caller-selected number of steps.

Trying to approximate those with module globals weakens isolation and usually
changes what the heap result means. Use the advanced protocol until the common
API can represent the lifecycle explicitly.

## When the common path does not fit

Use the YAML and adapter protocol when the subject depends on:

- a custom build plugin or generated code;
- a production-only transform that the project bundle cannot reproduce;
- an opaque runtime import;
- a user-owned server or protocol;
- a correctness verifier more expressive than `exact()`;
- a lifecycle the TypeScript case API cannot represent.

The protocol accepts a benchmark specification, variant descriptor, loopback
adapter, JSONL operations, and verifier process:

```text
bperf validate benchmark.yaml --variant variant.yaml
bperf measure benchmark.yaml variant.yaml --final-samples 20
```

That interface exposes more machinery because the caller has chosen to own it.
It should not leak back into ordinary benchmark modules.

## A real project example

[Benchmarking hls.js](HLS_JS.md) shows the complete setup for a CommonJS
project, including a remote MP4 fixture, a TypeScript benchmark, and the output
from a verified three-engine run.
