import assert from "node:assert/strict";
import test from "node:test";

import type {
  ArtifactEvidence,
  BrowserEvidence,
  EngineId,
  TrialCapture,
  TrialEngineAdapter,
  TrialLane,
  TrialRequest,
  WorkloadExecution,
} from "../src/contract.ts";
import { BrowserTrialLab } from "../src/trial.ts";

interface LaneCounts {
  opens: number;
  captures: number;
  closes: number;
}

function browser(rootPid: number): BrowserEvidence {
  return {
    root_pid: rootPid,
    executable_path: "browser",
    version: "1",
    launch_args: [],
  };
}

function workload(batchSize: number): WorkloadExecution {
  return {
    workload_wall_ms: 1,
    variant_call_wall_ms: 0.5,
    batch_wall_ms: batchSize,
    batch_size: batchSize,
    operation_count: 1,
    result: [{ value: 42 }],
  };
}

function artifact(
  kind: ArtifactEvidence["kind"],
): ArtifactEvidence {
  return {
    kind,
    path: `${kind}.json`,
    size_bytes: 1,
    sha256: "0".repeat(64),
    format: kind,
  };
}

function lane(
  counts: LaneCounts,
  capture: (request: TrialRequest) =>
    | TrialCapture
    | Promise<TrialCapture>,
): () => Promise<TrialLane<TrialCapture>> {
  return async () => {
    counts.opens += 1;
    return {
      async capture(request) {
        counts.captures += 1;
        return await capture(request);
      },
      async close() {
        counts.closes += 1;
      },
    };
  };
}

function adapter(
  counts: LaneCounts,
  failFirstCapture = false,
): TrialEngineAdapter {
  let captures = 0;
  return {
    openTrialLane: lane(counts, (request) => {
      captures += 1;
      if (failFirstCapture && captures === 1) {
        throw new Error("trial lane failed");
      }
      return {
        browser: browser(101),
        workload: workload(request.batchSize),
        cpu_active_ms: 0.75,
        js_heap_live_bytes: 4096,
        artifacts: [
          artifact("cpu_profile"),
          artifact("flamegraph"),
          artifact("js_heap"),
        ],
      };
    }),
  };
}

function request(): TrialRequest {
  return {
    targetUrl: "http://127.0.0.1:8080/",
    operations: [{ case_id: "fixture" }],
    artifactDirectory: "artifacts",
    browser: {
      viewport: { width: 800, height: 600 },
      locale: "en-US",
      timezone_id: "UTC",
      color_scheme: "light",
    },
    batchSize: 3,
    batchMaxSize: 10,
  };
}

function adapters(
  chromium: TrialEngineAdapter,
): Record<EngineId, TrialEngineAdapter> {
  return {
    chromium,
    firefox: chromium,
    webkit: chromium,
  };
}

test("one retained lane returns complete evidence for every trial", async () => {
  const counts = { opens: 0, captures: 0, closes: 0 };
  const lab = new BrowserTrialLab(adapters(adapter(counts)));

  const first = await lab.measureBrowserTrial("chromium", request());
  const second = await lab.measureBrowserTrial("chromium", request());

  assert.equal(first.browser.root_pid, 101);
  assert.equal(first.workload.batch_size, 3);
  assert.deepEqual(first.workload.result, [{ value: 42 }]);
  assert.deepEqual(
    first.artifacts.map(({ kind }) => kind),
    ["cpu_profile", "flamegraph", "js_heap"],
  );
  assert.equal(first.metrics["workload.wall_ms"], 1);
  assert.equal(
    first.metrics["browser.cpu_profile.active_ms"],
    0.75,
  );
  assert.equal(
    first.metrics["browser.js_heap.live_bytes"],
    4096,
  );
  assert.equal(second.workload.batch_size, 3);
  assert.deepEqual(counts, {
    opens: 1,
    captures: 2,
    closes: 0,
  });

  await lab.close();
  assert.deepEqual(counts, {
    opens: 1,
    captures: 2,
    closes: 1,
  });
});

test("a failed capture closes its lane before the next attempt", async () => {
  const counts = { opens: 0, captures: 0, closes: 0 };
  const lab = new BrowserTrialLab(
    adapters(adapter(counts, true)),
  );

  await assert.rejects(
    lab.measureBrowserTrial("chromium", request()),
    /trial lane failed/,
  );
  assert.deepEqual(counts, {
    opens: 1,
    captures: 1,
    closes: 1,
  });

  const evidence = await lab.measureBrowserTrial(
    "chromium",
    request(),
  );
  assert.deepEqual(evidence.workload.result, [{ value: 42 }]);
  assert.deepEqual(counts, {
    opens: 2,
    captures: 2,
    closes: 1,
  });

  await lab.close();
  assert.deepEqual(counts, {
    opens: 2,
    captures: 2,
    closes: 2,
  });
});

test("shutdown reports a lane close failure after attempting it", async () => {
  const counts = { opens: 0, captures: 0, closes: 0 };
  const broken = adapter(counts);
  const openTrialLane = broken.openTrialLane;
  broken.openTrialLane = async () => {
    const trial = await openTrialLane();
    return {
      ...trial,
      async close() {
        await trial.close();
        throw new Error("trial close failed");
      },
    };
  };
  const lab = new BrowserTrialLab(adapters(broken));
  await lab.measureBrowserTrial("chromium", request());

  await assert.rejects(
    lab.close(),
    /browser lanes failed to close/,
  );
  assert.deepEqual(counts, {
    opens: 1,
    captures: 1,
    closes: 1,
  });
});
