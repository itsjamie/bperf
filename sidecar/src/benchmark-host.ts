#!/usr/bin/env node

import crypto from "node:crypto";
import fs from "node:fs";
import http, {
  type IncomingMessage,
  type ServerResponse,
} from "node:http";
import path from "node:path";
import { once } from "node:events";
import { isDeepStrictEqual } from "node:util";
import { fileURLToPath, pathToFileURL } from "node:url";

import {
  chromium,
  firefox,
  webkit,
  type Browser,
  type BrowserType,
  type Page,
} from "playwright";

import type {
  BrowserBenchmarkDescription,
  FixtureDescriptor,
} from "./browser-benchmark.ts";
import type { EngineId } from "./contract.ts";
import {
  openBrowserProject,
  type BrowserProject,
} from "./project-modules.ts";
import { enforceNetworkPolicy } from "./trial-workload.ts";

const browserTypes = {
  chromium,
  firefox,
  webkit,
} satisfies Record<EngineId, BrowserType>;

const browserSdkPath = fileURLToPath(
  new URL("./browser-benchmark.ts", import.meta.url),
);
const benchmarkEntryRoute = "/__bperf/benchmark.js";

interface FixtureLockEntry {
  descriptor: FixtureDescriptor;
  source_url?: string;
  final_url?: string;
  body_path: string;
  sha256: string;
  size_bytes: number;
  content_type: string;
}

interface FixtureLock {
  schema_version: 1;
  fixtures: FixtureLockEntry[];
}

export interface ManagedBenchmarkDescription {
  schema_version: 1;
  benchmark_id: string;
  cases: BrowserBenchmarkDescription["cases"];
  source_files: string[];
  fixture_files: string[];
  fixture_lock: string;
}

export interface BenchmarkHostOptions {
  root: string;
  benchmark: string;
  fixtureLock?: string;
}

export interface BenchmarkHost {
  readonly origin: string;
  readonly sourceFiles: ReadonlySet<string>;
  close(): Promise<void>;
}

function pageDocument(): string {
  const entry = JSON.stringify(benchmarkEntryRoute);
  return `<!doctype html>
<meta charset="utf-8">
<title>bperf managed benchmark</title>
<script type="importmap">
{"imports":{"bperf/browser":"/__bperf/browser.ts"}}
</script>
<script type="module">
const benchmark = await import(${entry});
if (benchmark.default !== globalThis.__bperfDefinition) {
  throw new Error(
    "default export must be created with defineBrowserBenchmark",
  );
}
</script>`;
}

function readFixtureLock(filePath: string | undefined): Map<string, FixtureLockEntry> {
  if (!filePath) return new Map();
  const value = JSON.parse(fs.readFileSync(filePath, "utf8")) as FixtureLock;
  if (value.schema_version !== 1 || !Array.isArray(value.fixtures)) {
    throw new Error(`invalid fixture lock: ${filePath}`);
  }
  return new Map(
    value.fixtures.map((entry) => [
      JSON.stringify(entry.descriptor),
      entry,
    ]),
  );
}

function pinnedFixture(
  lock: Map<string, FixtureLockEntry>,
  descriptor: FixtureDescriptor,
): FixtureLockEntry | undefined {
  const entry = lock.get(JSON.stringify(descriptor));
  if (!entry?.source_url) return undefined;
  const body = fs.readFileSync(entry.body_path);
  if (
    body.length !== entry.size_bytes ||
    sha256(body) !== entry.sha256
  ) {
    throw new Error(
      `pinned remote fixture is missing or corrupt: ${descriptor.source}`,
    );
  }
  return entry;
}

function rangeFor(
  request: IncomingMessage,
  size: number,
): { start: number; end: number } | undefined {
  const value = request.headers.range;
  if (!value) return undefined;
  const match = /^bytes=(\d*)-(\d*)$/.exec(value);
  if (!match) {
    throw new Error("fixture range must contain one byte interval");
  }
  let start = match[1] ? Number(match[1]) : undefined;
  let end = match[2] ? Number(match[2]) : undefined;
  if (start === undefined && end === undefined) {
    throw new Error("fixture range is empty");
  }
  if (start === undefined) {
    const suffix = Math.min(end ?? 0, size);
    start = size - suffix;
    end = size - 1;
  } else {
    end = Math.min(end ?? size - 1, size - 1);
  }
  if (
    !Number.isSafeInteger(start) ||
    !Number.isSafeInteger(end) ||
    start < 0 ||
    end < start ||
    start >= size
  ) {
    throw new Error("fixture range is outside the response body");
  }
  return { start, end };
}

async function writeChunks(
  response: ServerResponse,
  body: Buffer,
  chunkSize: number,
  intervalMs: number,
): Promise<void> {
  for (let offset = 0; offset < body.length; offset += chunkSize) {
    const chunk = body.subarray(offset, offset + chunkSize);
    if (!response.write(chunk)) {
      await once(response, "drain");
    }
    if (intervalMs > 0 && offset + chunkSize < body.length) {
      await new Promise((resolve) => setTimeout(resolve, intervalMs));
    }
  }
  response.end();
}

async function serveFixture(
  request: IncomingMessage,
  response: ServerResponse,
  lock: Map<string, FixtureLockEntry>,
  requestUrl: URL,
): Promise<void> {
  const descriptor = requestUrl.searchParams.get("descriptor");
  const entry = descriptor ? lock.get(descriptor) : undefined;
  if (!entry) {
    response.writeHead(404).end("Unknown benchmark fixture");
    return;
  }

  const completeBody = fs.readFileSync(entry.body_path);
  let range;
  try {
    range = rangeFor(request, completeBody.length);
  } catch {
    response.writeHead(416, {
      "content-range": `bytes */${completeBody.length}`,
    }).end();
    return;
  }
  const body = range
    ? completeBody.subarray(range.start, range.end + 1)
    : completeBody;
  const headers: Record<string, string | number> = {
    "accept-ranges": "bytes",
    "cache-control": "no-store",
    "content-length": body.length,
    "content-type":
      entry.descriptor.response?.contentType ?? entry.content_type,
  };
  if (range) {
    headers["content-range"] =
      `bytes ${range.start}-${range.end}/${completeBody.length}`;
  }
  response.writeHead(range ? 206 : 200, headers);
  if (request.method === "HEAD") {
    response.end();
    return;
  }

  const stream = entry.descriptor.response?.stream;
  if (stream) {
    await writeChunks(
      response,
      body,
      stream.chunkSize,
      stream.intervalMs ?? 0,
    );
  } else {
    response.end(body);
  }
}

export async function startBenchmarkHost(
  options: BenchmarkHostOptions,
): Promise<BenchmarkHost> {
  const project = openBrowserProject(options.root, {
    virtualImports: ["bperf/browser"],
  });
  const benchmark = project.resolveFile(
    options.benchmark,
    "benchmark module",
  );
  const fixtureLock = readFixtureLock(options.fixtureLock);
  const entrySource = await project.browserEntry(benchmark);

  const server = http.createServer((request, response) => {
    void (async () => {
      const requestUrl = new URL(
        request.url ?? "/",
        "http://127.0.0.1",
      );
      if (requestUrl.pathname === "/") {
        response.writeHead(200, {
          "cache-control": "no-store",
          "content-type": "text/html; charset=utf-8",
        }).end(pageDocument());
        return;
      }
      if (requestUrl.pathname === "/__bperf/browser.ts") {
        response.writeHead(200, {
          "cache-control": "no-store",
          "content-type": "text/javascript; charset=utf-8",
        }).end(project.browserModule(browserSdkPath));
        return;
      }
      if (requestUrl.pathname === "/__bperf/fixture") {
        await serveFixture(request, response, fixtureLock, requestUrl);
        return;
      }
      if (requestUrl.pathname === benchmarkEntryRoute) {
        response.writeHead(200, {
          "cache-control": "no-store",
          "content-type": "text/javascript; charset=utf-8",
        }).end(entrySource);
        return;
      }
      response.writeHead(404).end("Not found");
    })().catch((error) => {
      response.writeHead(500, {
        "content-type": "text/plain; charset=utf-8",
      }).end(error instanceof Error ? error.message : String(error));
    });
  });
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      server.off("error", reject);
      resolve();
    });
  });
  const address = server.address();
  if (!address || typeof address === "string") {
    server.close();
    throw new Error("benchmark host did not expose a TCP port");
  }

  return {
    origin: `http://127.0.0.1:${address.port}`,
    sourceFiles: project.sourceFiles,
    async close() {
      await new Promise<void>((resolve, reject) => {
        server.close((error) => (error ? reject(error) : resolve()));
      });
    },
  };
}

async function inspectInEveryEngine(
  origin: string,
  exerciseCases = false,
): Promise<BrowserBenchmarkDescription> {
  let shared: BrowserBenchmarkDescription | undefined;
  for (const [engine, browserType] of Object.entries(browserTypes)) {
    const browser = await browserType.launch({ headless: true });
    try {
      const description = await withBenchmarkPage(
        browser,
        origin,
        (_page, description) => description,
      );
      if (shared && !isDeepStrictEqual(shared, description)) {
        throw new Error(
          `${engine} registered a different benchmark definition`,
        );
      }
      if (exerciseCases) {
        for (const benchmarkCase of description.cases) {
          await withBenchmarkPage(
            browser,
            origin,
            async (page, isolatedDescription) => {
              if (!isDeepStrictEqual(description, isolatedDescription)) {
                throw new Error(
                  `${engine} changed its benchmark definition between cases`,
                );
              }
              await exerciseBenchmarkCase(page, benchmarkCase, engine);
            },
          );
        }
      }
      shared = description;
    } catch (error) {
      throw new Error(`${engine} benchmark discovery failed`, {
        cause: error,
      });
    } finally {
      await browser.close({
        reason: "bperf benchmark discovery complete",
      });
    }
  }
  if (!shared) {
    throw new Error("benchmark discovery returned no engine descriptions");
  }
  return shared;
}

async function withBenchmarkPage<Result>(
  browser: Browser,
  origin: string,
  action: (
    page: Page,
    description: BrowserBenchmarkDescription,
  ) => Result | Promise<Result>,
): Promise<Result> {
  const context = await browser.newContext();
  try {
    await enforceNetworkPolicy(context);
    const page = await context.newPage();
    const loadFailures: string[] = [];
    page.on("pageerror", (error) => {
      loadFailures.push(error.message);
    });
    page.on("response", (response) => {
      if (response.status() >= 400) {
        loadFailures.push(
          `HTTP ${response.status()} loading ${response.url()}`,
        );
      }
    });
    await page.goto(origin, { waitUntil: "load" });
    try {
      await page.waitForFunction(
        () => Boolean(globalThis.__bperfDescription && globalThis.__bperf),
        undefined,
        { timeout: 10_000 },
      );
    } catch (cause) {
      const details = loadFailures.length
        ? `: ${loadFailures.join("; ")}`
        : "";
      throw new Error(`benchmark page did not register${details}`, {
        cause,
      });
    }
    const description = await page.evaluate(
      () => globalThis.__bperfDescription,
    );
    if (!description) {
      throw new Error("browser returned no benchmark description");
    }
    return await action(page, description);
  } finally {
    await context.close();
  }
}

async function exerciseBenchmarkCase(
  page: Page,
  benchmarkCase: BrowserBenchmarkDescription["cases"][number],
  engine: string,
): Promise<void> {
  const operations = [{ case_id: benchmarkCase.id }];
  const results = await page.evaluate(async (values) => {
    const adapter = globalThis.__bperf;
    if (!adapter) {
      throw new Error("benchmark page adapter was not installed");
    }
    await adapter.prepare(values);
    const results = [];
    for (const value of values) {
      results.push(await adapter.run(value));
    }
    await adapter.settle();
    return results;
  }, operations);
  if (!isDeepStrictEqual(results[0], benchmarkCase.expectation.value)) {
    throw new Error(
      `${engine} case ${JSON.stringify(benchmarkCase.id)} returned ${
        JSON.stringify(results[0])
      }; expected ${JSON.stringify(benchmarkCase.expectation.value)}`,
    );
  }
}

function sha256(body: Buffer): string {
  return crypto.createHash("sha256").update(body).digest("hex");
}

function contentType(source: string): string {
  switch (path.extname(new URL(source, "file:///").pathname).toLowerCase()) {
    case ".json":
      return "application/json";
    case ".m3u8":
      return "application/vnd.apple.mpegurl";
    case ".mp4":
    case ".m4s":
      return "video/mp4";
    case ".txt":
      return "text/plain; charset=utf-8";
    default:
      return "application/octet-stream";
  }
}

async function acquireFixture(
  descriptor: FixtureDescriptor,
  benchmarkDirectory: string,
  project: BrowserProject,
  cacheRoot: string,
): Promise<FixtureLockEntry> {
  let body: Buffer;
  let sourceUrl: string | undefined;
  let finalUrl: string | undefined;
  let responseContentType = contentType(descriptor.source);

  let parsed: URL | undefined;
  try {
    parsed = new URL(descriptor.source);
  } catch {
    parsed = undefined;
  }
  if (parsed && ["http:", "https:"].includes(parsed.protocol)) {
    sourceUrl = parsed.href;
    const response = await fetch(parsed);
    if (!response.ok) {
      throw new Error(
        `remote fixture ${parsed.href} returned HTTP ${response.status}`,
      );
    }
    body = Buffer.from(await response.arrayBuffer());
    finalUrl = response.url;
    responseContentType =
      response.headers.get("content-type") ?? responseContentType;
  } else {
    const filePath = project.resolveFile(
      path.resolve(benchmarkDirectory, descriptor.source),
      "fixture",
    );
    body = fs.readFileSync(filePath);
  }

  const digest = sha256(body);
  fs.mkdirSync(cacheRoot, { recursive: true });
  const bodyPath = path.join(cacheRoot, digest);
  if (!fs.existsSync(bodyPath)) {
    fs.writeFileSync(bodyPath, body, { flag: "wx" });
  }
  return {
    descriptor,
    ...(sourceUrl ? { source_url: sourceUrl } : {}),
    ...(finalUrl ? { final_url: finalUrl } : {}),
    body_path: fs.realpathSync(bodyPath),
    sha256: digest,
    size_bytes: body.length,
    content_type: responseContentType,
  };
}

export async function describeBenchmark(
  options: BenchmarkHostOptions & {
    fixtureLock: string;
    fixtureCache: string;
  },
): Promise<ManagedBenchmarkDescription> {
  const project = openBrowserProject(options.root);
  const root = project.root;
  const benchmark = project.resolveFile(
    options.benchmark,
    "benchmark module",
  );
  const discoveryHost = await startBenchmarkHost({ root, benchmark });
  let description: BrowserBenchmarkDescription;
  let discoverySources: string[];
  try {
    description = await inspectInEveryEngine(discoveryHost.origin);
    discoverySources = [...discoveryHost.sourceFiles];
  } finally {
    await discoveryHost.close();
  }

  const existing = fs.existsSync(options.fixtureLock)
    ? readFixtureLock(options.fixtureLock)
    : new Map<string, FixtureLockEntry>();
  const entries = [];
  for (const descriptor of description.fixtures) {
    entries.push(
      pinnedFixture(existing, descriptor) ??
        await acquireFixture(
          descriptor,
          path.dirname(benchmark),
          project,
          options.fixtureCache,
        ),
    );
  }
  const lock: FixtureLock = {
    schema_version: 1,
    fixtures: entries,
  };
  fs.mkdirSync(path.dirname(options.fixtureLock), { recursive: true });
  fs.writeFileSync(
    options.fixtureLock,
    `${JSON.stringify(lock, null, 2)}\n`,
  );

  const resolvedHost = await startBenchmarkHost({
    root,
    benchmark,
    fixtureLock: options.fixtureLock,
  });
  try {
    const resolvedDescription = await inspectInEveryEngine(
      resolvedHost.origin,
      true,
    );
    if (!isDeepStrictEqual(description, resolvedDescription)) {
      throw new Error(
        "benchmark definition changed after fixtures were resolved",
      );
    }
    return {
      schema_version: 1,
      benchmark_id: description.id,
      cases: description.cases,
      source_files: [
        ...new Set([
          ...discoverySources,
          ...resolvedHost.sourceFiles,
        ]),
      ].sort(),
      fixture_files: entries.map((entry) => entry.body_path).sort(),
      fixture_lock: fs.realpathSync(options.fixtureLock),
    };
  } finally {
    await resolvedHost.close();
  }
}

interface ParsedArguments {
  mode: "describe" | "serve";
  benchmark: string;
  root: string;
  fixtureLock: string;
  fixtureCache?: string;
}

function parseArguments(values: string[]): ParsedArguments {
  const [mode, benchmark, ...rest] = values;
  if (!["describe", "serve"].includes(mode) || !benchmark) {
    throw new Error(
      "usage: benchmark-host.ts <describe|serve> <benchmark> --root <path> --lock <path> [--cache <path>]",
    );
  }
  const options = new Map<string, string>();
  for (let index = 0; index < rest.length; index += 2) {
    const key = rest[index];
    const value = rest[index + 1];
    if (!key?.startsWith("--") || !value) {
      throw new Error("benchmark host options require --name value pairs");
    }
    options.set(key.slice(2), value);
  }
  const root = options.get("root");
  const fixtureLock = options.get("lock");
  if (!root || !fixtureLock) {
    throw new Error("benchmark host requires --root and --lock");
  }
  return {
    mode: mode as ParsedArguments["mode"],
    benchmark,
    root,
    fixtureLock,
    ...(options.get("cache")
      ? { fixtureCache: options.get("cache") }
      : {}),
  };
}

async function main(): Promise<void> {
  const options = parseArguments(process.argv.slice(2));
  if (options.mode === "describe") {
    if (!options.fixtureCache) {
      throw new Error("benchmark description requires --cache");
    }
    const description = await describeBenchmark({
      root: options.root,
      benchmark: options.benchmark,
      fixtureLock: options.fixtureLock,
      fixtureCache: options.fixtureCache,
    });
    process.stdout.write(`${JSON.stringify(description)}\n`);
    return;
  }

  const host = await startBenchmarkHost({
    root: options.root,
    benchmark: options.benchmark,
    fixtureLock: options.fixtureLock,
  });
  process.stdout.write(
    `${JSON.stringify({
      protocol_version: 1,
      url: `${host.origin}/`,
    })}\n`,
  );
  const close = async () => {
    await host.close();
    process.exit(0);
  };
  process.once("SIGTERM", () => void close());
  process.once("SIGINT", () => void close());
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(process.argv[1]).href
) {
  await main();
}
