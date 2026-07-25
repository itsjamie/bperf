import assert from "node:assert/strict";
import test from "node:test";

import {
  defineBrowserBenchmark,
  exact,
  fixture,
} from "../src/browser-benchmark.ts";

Object.defineProperty(globalThis, "location", {
  configurable: true,
  value: new URL("http://127.0.0.1:4317"),
});

test("fixture exposes a URL without choosing a loading mechanism", () => {
  const resource = fixture("./fixtures/segment.mp4", {
    response: {
      contentType: "video/mp4",
      stream: { chunkSize: 4096, intervalMs: 2 },
    },
  });

  assert.equal(resource.url.origin, "http://127.0.0.1:4317");
  assert.equal(resource.url.pathname, "/__bperf/fixture");
  assert.deepEqual(
    JSON.parse(resource.url.searchParams.get("descriptor") ?? ""),
    {
      source: "./fixtures/segment.mp4",
      response: {
        contentType: "video/mp4",
        stream: { chunkSize: 4096, intervalMs: 2 },
      },
    },
  );

  defineBrowserBenchmark({
    id: "fixture-url",
    cases: [
      {
        id: "fetch",
        measure: () => null,
        expect: exact(null),
      },
    ],
  });
  assert.deepEqual(globalThis.__bperfDescription?.fixtures, [
    {
      source: "./fixtures/segment.mp4",
      response: {
        contentType: "video/mp4",
        stream: { chunkSize: 4096, intervalMs: 2 },
      },
    },
  ]);
});

test("setup and settle surround repeatable semantic measurements", async () => {
  let setupCount = 0;
  let measureCount = 0;
  let settleCount = 0;
  defineBrowserBenchmark({
    id: "fragment-parser",
    cases: [
      {
        id: "representative-fragment",
        setup() {
          setupCount += 1;
          return 41;
        },
        measure(value: number) {
          measureCount += 1;
          return { value: value + 1 };
        },
        settle() {
          settleCount += 1;
        },
        expect: exact({ value: 42 }),
      },
    ],
  });

  assert.deepEqual(globalThis.__bperfDescription, {
    id: "fragment-parser",
    cases: [
      {
        id: "representative-fragment",
        expectation: {
          kind: "exact",
          value: { value: 42 },
        },
      },
    ],
    fixtures: [],
  });

  await globalThis.__bperf?.prepare([
    { case_id: "representative-fragment" },
  ]);
  await globalThis.__bperf?.prepare([
    { case_id: "representative-fragment" },
  ]);
  assert.equal(setupCount, 1);
  assert.deepEqual(
    await globalThis.__bperf?.run({
      case_id: "representative-fragment",
    }),
    { value: 42 },
  );
  assert.deepEqual(
    await globalThis.__bperf?.run({
      case_id: "representative-fragment",
    }),
    { value: 42 },
  );
  await globalThis.__bperf?.settle();
  assert.equal(measureCount, 2);
  assert.equal(settleCount, 1);
});

test("exact snapshots expected values at the authoring boundary", () => {
  const expected = { nested: { value: 42 } };
  const expectation = exact(expected);
  expected.nested.value = 41;

  assert.deepEqual(expectation.value, { nested: { value: 42 } });
  assert.equal(Object.isFrozen(expectation.value), true);
  assert.equal(
    Object.isFrozen(
      (expectation.value as { nested: { value: number } }).nested,
    ),
    true,
  );
});

test("invalid fixture delivery fails while authoring the benchmark", () => {
  assert.throws(
    () =>
      fixture("./segment.mp4", {
        response: { stream: { chunkSize: 0 } },
      }),
    /chunkSize must be positive/,
  );
  assert.throws(
    () =>
      fixture("./segment.mp4", {
        response: { contentType: "" },
      }),
    /contentType must be a non-empty string/,
  );
});

test("duplicate cases fail while defining the benchmark", () => {
  const benchmarkCase = {
    id: "duplicate",
    measure: () => null,
    expect: exact(null),
  };

  assert.throws(
    () => defineBrowserBenchmark({
      id: "invalid",
      cases: [benchmarkCase, benchmarkCase],
    }),
    /duplicate benchmark case/,
  );
});
