import type {
  Browser,
  BrowserContext,
  BrowserContextOptions,
  Page,
} from "playwright";

import type {
  BrowserTrialConfig,
  TrialRequest,
  WorkloadExecution,
} from "./contract.ts";

interface BenchmarkPageAdapter {
  prepare?(operations: unknown[]): unknown | Promise<unknown>;
  run(operation: unknown): unknown | Promise<unknown>;
  settle?(): unknown | Promise<unknown>;
}

function isLoopback(hostname: string): boolean {
  return (
    hostname === "localhost" ||
    hostname === "::1" ||
    hostname === "[::1]" ||
    hostname === "127.0.0.1" ||
    hostname.startsWith("127.")
  );
}

export function isAllowedTrialUrl(value: string): boolean {
  try {
    const url = new URL(value);
    if (["data:", "blob:", "about:"].includes(url.protocol)) return true;
    return (
      ["http:", "https:", "ws:", "wss:"].includes(url.protocol) &&
      isLoopback(url.hostname)
    );
  } catch {
    return false;
  }
}

export function isAllowedAdapterUrl(value: string): boolean {
  try {
    const url = new URL(value);
    return (
      url.protocol === "http:" && isLoopback(url.hostname)
    );
  } catch {
    return false;
  }
}

export function contextOptions(
  config: BrowserTrialConfig,
): BrowserContextOptions {
  return {
    viewport: config.viewport,
    locale: config.locale,
    timezoneId: config.timezone_id,
    colorScheme: config.color_scheme,
  };
}

export async function enforceNetworkPolicy(
  context: BrowserContext,
): Promise<void> {
  await context.route("**/*", async (route) => {
    if (isAllowedTrialUrl(route.request().url())) {
      await route.continue();
    } else {
      await route.abort("blockedbyclient");
    }
  });
  await context.routeWebSocket("**/*", async (socket) => {
    if (isAllowedTrialUrl(socket.url())) {
      socket.connectToServer();
    } else {
      await socket.close({
        code: 1008,
        reason: "Blocked by bperf local-only policy",
      });
    }
  });
}

export async function prepareBenchmarkPage(
  page: Page,
  targetUrl: string,
  operations: unknown[],
): Promise<void> {
  if (!isAllowedAdapterUrl(targetUrl)) {
    throw new Error(`Variant adapter returned a non-loopback URL: ${targetUrl}`);
  }
  await page.goto(targetUrl, { waitUntil: "load" });
  await page.waitForFunction(
    () => {
      const target = globalThis as typeof globalThis & {
        __bperf?: BenchmarkPageAdapter;
      };
      return typeof target.__bperf?.run === "function";
    },
    undefined,
    { timeout: 10_000 },
  );
  await page.evaluate(async (values) => {
    const target = globalThis as typeof globalThis & {
      __bperf?: BenchmarkPageAdapter;
    };
    await target.__bperf?.prepare?.(values);
  }, operations);
}

export async function withPreparedTrialPage<Result>(
  browser: Browser,
  request: Pick<
    TrialRequest,
    "browser" | "targetUrl" | "operations"
  >,
  capture: (page: Page) => Result | Promise<Result>,
): Promise<Result> {
  const context = await browser.newContext(
    contextOptions(request.browser),
  );
  try {
    await enforceNetworkPolicy(context);
    const page = await context.newPage();
    await prepareBenchmarkPage(
      page,
      request.targetUrl,
      request.operations,
    );
    return await capture(page);
  } finally {
    await context.close();
  }
}

export async function executeBenchmarkWorkload(
  page: Page,
  operations: unknown[],
  batchSize = 1,
): Promise<WorkloadExecution> {
  if (!Number.isSafeInteger(batchSize) || batchSize <= 0) {
    throw new Error("benchmark batch size must be a positive safe integer");
  }
  return page.evaluate(
    async ({ values, repetitions }) => {
      // Aggregate short calls before reading clocks that quantize sub-ms spans.
      const measurementGroupSize = 32;
      const target = globalThis as typeof globalThis & {
        __bperf?: BenchmarkPageAdapter;
      };
      if (typeof target.__bperf?.run !== "function") {
        throw new Error("Page benchmark adapter has no run(operation) method");
      }

      const started = performance.now();
      let variantCallWallMs = 0;
      let result: unknown[] | undefined;
      let encodedResult: string | undefined;
      for (
        let groupStart = 0;
        groupStart < repetitions;
        groupStart += measurementGroupSize
      ) {
        const group = [];
        const groupEnd = Math.min(
          repetitions,
          groupStart + measurementGroupSize,
        );
        const callsStarted = performance.now();
        for (
          let repetition = groupStart;
          repetition < groupEnd;
          repetition += 1
        ) {
          const current = [];
          for (const operation of values) {
            current.push(await target.__bperf.run(operation));
          }
          group.push(current);
        }
        variantCallWallMs += performance.now() - callsStarted;

        for (const current of group) {
          const encoded = JSON.stringify(current);
          if (encoded === undefined) {
            throw new Error("benchmark result is not JSON-serializable");
          }
          if (encodedResult !== undefined && encoded !== encodedResult) {
            throw new Error(
              "batched benchmark invocations returned different results",
            );
          }
          result ??= current;
          encodedResult = encoded;
        }
      }
      const batchWallMs = performance.now() - started;
      return {
        workload_wall_ms: batchWallMs / repetitions,
        variant_call_wall_ms: variantCallWallMs / repetitions,
        batch_wall_ms: batchWallMs,
        batch_size: repetitions,
        operation_count: values.length,
        result: result ?? [],
      };
    },
    { values: operations, repetitions: batchSize },
  );
}

export async function selectBenchmarkBatchSize(
  page: Page,
  operations: unknown[],
  initialSize: number,
  targetMs: number | undefined,
  maximumSize: number,
): Promise<number> {
  if (targetMs === undefined) return initialSize;

  let batchSize = initialSize;
  while (true) {
    const workload = await executeBenchmarkWorkload(
      page,
      operations,
      batchSize,
    );
    if (
      workload.batch_wall_ms >= targetMs ||
      batchSize === maximumSize
    ) {
      return batchSize;
    }
    const estimated = workload.batch_wall_ms > 0
      ? Math.ceil(
          batchSize * targetMs / workload.batch_wall_ms,
        )
      : batchSize * 10;
    batchSize = Math.min(
      maximumSize,
      Math.max(batchSize + 1, estimated),
    );
  }
}

export async function settleBenchmarkPage(page: Page): Promise<void> {
  await page.evaluate(async () => {
    const target = globalThis as typeof globalThis & {
      __bperf?: BenchmarkPageAdapter;
    };
    await target.__bperf?.settle?.();
  });
}

export async function runBenchmarkWorkload(
  page: Page,
  targetUrl: string,
  operations: unknown[],
  batchSize = 1,
): Promise<WorkloadExecution> {
  await prepareBenchmarkPage(page, targetUrl, operations);
  const workload = await executeBenchmarkWorkload(
    page,
    operations,
    batchSize,
  );
  await settleBenchmarkPage(page);
  return workload;
}
