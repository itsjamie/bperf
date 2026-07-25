# Benchmarking hls.js

This walkthrough was verified against
[`video-dev/hls.js`](https://github.com/video-dev/hls.js) commit
`86e8b5f34172115b743b51dfa18ac57d13d33a45`.

That checkout is a CommonJS package context: `package.json` has no
`"type": "module"` and its JavaScript build configuration uses `require(...)`.
bperf does not require changing the package type, editing its TypeScript
configuration, building hls.js, or adding bperf as an npm dependency.

The benchmark can still use TypeScript and imports because bperf bundles the
entry for the browser without changing how Node loads the rest of hls.js.

## Prepare the checkout

From the bperf source checkout, install a complete local build:

```text
node scripts/package-release.ts --install
bperf --version
```

The installer places the executable in Cargo's user `bin` directory and keeps
its versioned Node sidecar beside it. Ensure that directory is on `PATH` before
continuing.

Prepare hls.js normally:

```text
git clone https://github.com/video-dev/hls.js.git
cd hls.js
git checkout 86e8b5f34172115b743b51dfa18ac57d13d33a45
npm ci
mkdir benchmarks
```

Ignore local measurement evidence:

```gitignore
.bperf/
```

## Add the benchmark

Save this as `benchmarks/mp4-tools.bench.ts`:

```ts
import {
  defineBrowserBenchmark,
  exact,
  fixture,
} from "bperf/browser";

import { ChunkMetadata } from "../src/types/transmuxer.ts";
import { logger } from "../src/utils/logger.ts";
import {
  getSampleData,
  parseInitSegment,
} from "../src/utils/mp4-tools.ts";

const mediaRoot =
  "https://storage.googleapis.com/shaka-demo-assets/angel-one-hls";
const initSegment = fixture(
  `${mediaRoot}/v-0576p-1400k-libx264-init.mp4`,
);
const mediaSegment = fixture(
  `${mediaRoot}/v-0576p-1400k-libx264-s1.mp4`,
);

async function loadBytes(url: URL): Promise<Uint8Array<ArrayBuffer>> {
  const response = await fetch(url);
  if (!response.ok) {
    throw new Error(`fixture returned HTTP ${response.status}`);
  }
  return new Uint8Array(await response.arrayBuffer());
}

export default defineBrowserBenchmark({
  id: "hls-mp4-sample-data",

  cases: [
    {
      id: "angel-one-video-fragment",

      async setup() {
        const [initBytes, mediaBytes] = await Promise.all([
          loadBytes(initSegment.url),
          loadBytes(mediaSegment.url),
        ]);
        return {
          initData: parseInitSegment(initBytes),
          mediaBytes,
          metadata: new ChunkMetadata(
            0,
            1,
            0,
            mediaBytes.byteLength,
          ),
        };
      },

      measure({ initData, mediaBytes, metadata }) {
        const tracks = getSampleData(
          mediaBytes,
          initData,
          metadata,
          logger,
        );
        return {
          byteLength: mediaBytes.byteLength,
          tracks: Object.entries(tracks).map(([id, track]) => ({
            id: Number(id),
            type: track.type,
            sampleCount: track.sampleCount,
            runCount: track.trun.length,
            duration: track.duration,
            timescale: track.timescale,
            start: track.start,
            keyFrameIndex: track.keyFrameIndex ?? null,
            ptsMin: track.ptsMin ?? null,
            ptsMax: track.ptsMax ?? null,
          })),
        };
      },

      expect: exact({
        byteLength: 3836611,
        tracks: [
          {
            id: 1,
            type: "video",
            sampleCount: 100,
            runCount: 1,
            duration: 51200,
            timescale: 12800,
            start: 0,
            keyFrameIndex: 0,
            ptsMin: 0,
            ptsMax: 51200,
          },
        ],
      }),
    },
  ],
});
```

The two remote MP4 files are acquired before measurement, stored by content
hash under `.bperf/objects/`, and pinned in this benchmark's fixture lock. Every
browser receives the pinned bytes from bperf's loopback server.

## Run it

Run the capability gate once, then the benchmark:

```text
bperf doctor --engine all
bperf run benchmarks/mp4-tools.bench.ts --budget 1s
```

The one-second budget is deliberately smaller than the minimum evidence floor.
It demonstrates that the budget is a target, not a deadline that can weaken the
result.

On the verification machine, the clean checkout produced:

```text
bperf run: measured
  76/76 trials recorded
  adaptive calibration: 16 pilot trials; 3/3 strata met the stability rule
  adaptive sampling: 60 final trials
  artifacts: 9 representative retained, 219 discarded
  profiles: .bperf/measurements/<measurement-id>/artifact-retention.json
  sampling: .bperf/measurements/<measurement-id>/sampling.json
  measurement: .bperf/measurements/<measurement-id>/summary.json
  chromium: measured correctness=pass final=20/20 invalid_attempts=0
  firefox: measured correctness=pass final=20/20 invalid_attempts=0
  webkit: measured correctness=pass final=20/20 invalid_attempts=0
  comparison: no promoted baseline
  cycle: cycle-<id>
  source change: bperf show cycle-<id> --diff
  measurement index: .bperf/measurements/index/<receipt>.json
```

Pilot counts are selected independently and can vary by machine. This run
contained 20 complete final samples in each engine. Every final sample included
timing, CPU, flamegraph, heap, and correctness evidence from the same workload
execution.

The verification run was interrupted once and then resumed with the same
command. bperf kept its valid trials and completed the immutable schedule.

After the run finishes, the browser-bundled source graph and the project files
that control its resolution form the variant identity. The hls.js package type
remains CommonJS; that detail is handled inside the project-bundle boundary
rather than exposed in the benchmark API.
