import assert from "node:assert/strict";
import test from "node:test";

import type {
  Browser,
  BrowserContext,
  Page,
} from "playwright";

import {
  contextOptions,
  isAllowedAdapterUrl,
  isAllowedTrialUrl,
  selectBenchmarkBatchSize,
  withPreparedTrialPage,
} from "../src/trial-workload.ts";

test("trial URLs are restricted to local browser fixtures", () => {
  assert.equal(isAllowedTrialUrl("http://127.0.0.1:8080/"), true);
  assert.equal(isAllowedTrialUrl("http://localhost/fixture"), true);
  assert.equal(isAllowedTrialUrl("ws://127.0.0.1:8080/events"), true);
  assert.equal(isAllowedTrialUrl("data:text/html,fixture"), true);
  assert.equal(isAllowedTrialUrl("https://example.com/"), false);
  assert.equal(isAllowedTrialUrl("wss://example.com/events"), false);
  assert.equal(isAllowedTrialUrl("not a url"), false);
});

test("adapter targets must be loopback HTTP fixtures", () => {
  assert.equal(isAllowedAdapterUrl("http://127.0.0.1:8080/"), true);
  assert.equal(isAllowedAdapterUrl("https://localhost/fixture"), false);
  assert.equal(isAllowedAdapterUrl("data:text/html,fixture"), false);
  assert.equal(isAllowedAdapterUrl("ws://127.0.0.1/events"), false);
});

test("browser context options preserve neutral benchmark settings", () => {
  assert.deepEqual(
    contextOptions({
      viewport: { width: 800, height: 600 },
      locale: "en-CA",
      timezone_id: "UTC",
      color_scheme: "dark",
    }),
    {
      viewport: { width: 800, height: 600 },
      locale: "en-CA",
      timezoneId: "UTC",
      colorScheme: "dark",
    },
  );
});

test("calibration selects one bounded batch for the captured workload", async () => {
  const attempted: number[] = [];
  const page = {
    async evaluate(
      _callback: unknown,
      input: { repetitions: number },
    ) {
      attempted.push(input.repetitions);
      const batchWallMs = input.repetitions * 2;
      return {
        workload_wall_ms: 2,
        variant_call_wall_ms: 1,
        batch_wall_ms: batchWallMs,
        batch_size: input.repetitions,
        operation_count: 1,
        result: [{ value: 42 }],
      };
    },
  } as unknown as Page;

  const selected = await selectBenchmarkBatchSize(
    page,
    [{ case_id: "fixture" }],
    1,
    10,
    20,
  );

  assert.equal(selected, 5);
  assert.deepEqual(attempted, [1, 5]);
});

test("locked final batches do not run sizing probes", async () => {
  const page = {
    async evaluate() {
      throw new Error("unexpected sizing probe");
    },
  } as unknown as Page;

  assert.equal(
    await selectBenchmarkBatchSize(
      page,
      [{ case_id: "fixture" }],
      7,
      undefined,
      20,
    ),
    7,
  );
});

test("every prepared page receives a context that closes after use", async () => {
  const events: string[] = [];
  const page = {
    async goto() {
      events.push("goto");
    },
    async waitForFunction() {
      events.push("ready");
    },
    async evaluate() {
      events.push("prepare");
    },
  } as unknown as Page;
  const browser = {
    async newContext() {
      events.push("context");
      return {
        async route() {},
        async routeWebSocket() {},
        async newPage() {
          events.push("page");
          return page;
        },
        async close() {
          events.push("close");
        },
      } as unknown as BrowserContext;
    },
  } as unknown as Browser;
  const trial = {
    targetUrl: "http://127.0.0.1:8080/",
    operations: [{ case_id: "fixture" }],
    browser: {
      viewport: { width: 800, height: 600 },
      locale: "en-US",
      timezone_id: "UTC",
      color_scheme: "light" as const,
    },
  };

  await withPreparedTrialPage(browser, trial, async () => {
    events.push("capture");
  });
  await withPreparedTrialPage(browser, trial, async () => {
    events.push("capture");
  });

  assert.deepEqual(events, [
    "context",
    "page",
    "goto",
    "ready",
    "prepare",
    "capture",
    "close",
    "context",
    "page",
    "goto",
    "ready",
    "prepare",
    "capture",
    "close",
  ]);
});

test("prepared page contexts close when capture fails", async () => {
  let closes = 0;
  const browser = {
    async newContext() {
      return {
        async route() {},
        async routeWebSocket() {},
        async newPage() {
          return {
            async goto() {},
            async waitForFunction() {},
            async evaluate() {},
          } as unknown as Page;
        },
        async close() {
          closes += 1;
        },
      } as unknown as BrowserContext;
    },
  } as unknown as Browser;

  await assert.rejects(
    withPreparedTrialPage(
      browser,
      {
        targetUrl: "http://127.0.0.1:8080/",
        operations: [{ case_id: "fixture" }],
        browser: {
          viewport: { width: 800, height: 600 },
          locale: "en-US",
          timezone_id: "UTC",
          color_scheme: "light",
        },
      },
      async () => {
        throw new Error("capture failed");
      },
    ),
    /capture failed/,
  );
  assert.equal(closes, 1);
});
