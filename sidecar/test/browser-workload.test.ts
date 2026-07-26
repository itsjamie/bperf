import assert from "node:assert/strict";
import fs from "node:fs";
import test from "node:test";
import vm from "node:vm";

const BROWSER_WORKLOAD_VERSION = 1;
const BROWSER_WORKLOAD_SOURCE = fs.readFileSync(
  new URL("../src/browser-workload.js", import.meta.url),
  "utf8",
);

interface Harness {
  version: number;
  selectBatchSize(
    operations: unknown[],
    initial: number,
    target: number | undefined,
    maximum: number,
  ): Promise<number>;
}

function harness(run: (operation: unknown) => unknown): Harness {
  let now = 0;
  const context = {
    performance: {
      now() {
        return now;
      },
    },
    __bperf: {
      run(operation: unknown) {
        now += 2;
        return run(operation);
      },
    },
  };
  vm.runInNewContext(BROWSER_WORKLOAD_SOURCE, context);
  return (context as typeof context & { __bperfHarness: Harness })
    .__bperfHarness;
}

test("shared browser workload selects one bounded captured batch", async () => {
  const attempted: unknown[] = [];
  const workload = harness((operation) => {
    attempted.push(operation);
    return { value: 42 };
  });

  assert.equal(workload.version, BROWSER_WORKLOAD_VERSION);
  assert.equal(
    await workload.selectBatchSize(
      [{ case_id: "fixture" }],
      1,
      10,
      20,
    ),
    5,
  );
  assert.equal(attempted.length, 6);
});

test("shared browser workload does not size a locked final batch", async () => {
  let calls = 0;
  const workload = harness(() => {
    calls += 1;
    return 42;
  });

  assert.equal(
    await workload.selectBatchSize(
      [{ case_id: "fixture" }],
      7,
      undefined,
      20,
    ),
    7,
  );
  assert.equal(calls, 0);
});
