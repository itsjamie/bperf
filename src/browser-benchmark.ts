// Values crossing the browser/host boundary must remain JSON-compatible.
export type JsonValue =
  | null
  | boolean
  | number
  | string
  | readonly JsonValue[]
  | { readonly [key: string]: JsonValue };

export type EngineId = "chromium" | "firefox" | "webkit";

export type GpuApi = "webgl" | "webgl2" | "webgpu";

export interface FixtureResponse {
  contentType?: string;
  stream?: {
    chunkSize: number;
    intervalMs?: number;
  };
}

export interface FixtureOptions {
  response?: FixtureResponse;
}

export interface BrowserFixture {
  readonly url: URL;
}

export interface ExactExpectation {
  readonly kind: "exact";
  readonly value: JsonValue;
}

export interface BrowserBenchmarkCase<State = unknown> {
  id: string;
  setup?(): State | Promise<State>;
  measure(state: NoInfer<State>): JsonValue | Promise<JsonValue>;
  settle?(state: State): void | Promise<void>;
  expect: ExactExpectation;
}

export interface BrowserBenchmarkDefinition<
  States extends readonly unknown[] = readonly unknown[],
> {
  id: string;
  // Omitting the scope selects every engine. A narrower scope is part of the
  // benchmark's identity, and its results say nothing about excluded engines.
  engines?: readonly EngineId[];
  // Launches GPU-enabled lanes for this API and records the adapter each
  // engine rendered with. Every engine in scope must implement it.
  gpu?: GpuApi;
  cases: {
    readonly [Index in keyof States]: BrowserBenchmarkCase<States[Index]>;
  };
}

export interface FixtureDescriptor {
  source: string;
  response?: FixtureResponse;
}

export interface BrowserBenchmarkDescription {
  id: string;
  engines: EngineId[];
  gpu: GpuApi | null;
  cases: Array<{
    id: string;
    expectation: ExactExpectation;
  }>;
  fixtures: FixtureDescriptor[];
}

interface BenchmarkOperation {
  case_id: string;
}

interface BenchmarkPageAdapter {
  prepare(operations: unknown[]): Promise<void>;
  run(operation: unknown): Promise<JsonValue>;
  settle(): Promise<void>;
}

interface RegisteredBenchmark {
  id: string;
  engines: EngineId[];
  gpu: GpuApi | null;
  cases: Map<string, BrowserBenchmarkCase<unknown>>;
  fixtures: FixtureDescriptor[];
}

declare global {
  var __bperf: BenchmarkPageAdapter | undefined;
  var __bperfDescription: BrowserBenchmarkDescription | undefined;
  var __bperfDefinition: object | undefined;
}

const ENGINES: readonly EngineId[] = ["chromium", "firefox", "webkit"];
const GPU_APIS: readonly GpuApi[] = ["webgl", "webgl2", "webgpu"];
const fixtureDescriptors = new Map<string, FixtureDescriptor>();
const exactExpectations = new WeakSet<object>();

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function requireIdentifier(label: string, value: unknown): string {
  if (
    typeof value !== "string" ||
    value.length === 0 ||
    !/^[A-Za-z0-9_.-]+$/.test(value)
  ) {
    throw new Error(
      `${label} must contain only letters, digits, dot, dash, or underscore`,
    );
  }
  return value;
}

function engineScope(value: unknown): EngineId[] {
  if (value === undefined) return [...ENGINES];
  if (!Array.isArray(value) || value.length === 0) {
    throw new Error("benchmark engines must be a non-empty array");
  }
  const selected = new Set<EngineId>();
  for (const engine of value) {
    if (!ENGINES.includes(engine)) {
      throw new Error(
        `benchmark engines contains unknown engine ${JSON.stringify(engine)}`,
      );
    }
    if (selected.has(engine)) {
      throw new Error(
        `benchmark engines contains duplicate engine ${JSON.stringify(engine)}`,
      );
    }
    selected.add(engine);
  }
  return ENGINES.filter((engine) => selected.has(engine));
}

function gpuRequirement(value: unknown): GpuApi | null {
  if (value === undefined) return null;
  if (!GPU_APIS.includes(value as GpuApi)) {
    throw new Error(
      `benchmark gpu must be one of ${GPU_APIS.join(", ")}`,
    );
  }
  return value as GpuApi;
}

function requireJson(value: unknown, path = "value"): asserts value is JsonValue {
  if (
    value === null ||
    typeof value === "boolean" ||
    typeof value === "string"
  ) {
    return;
  }
  if (typeof value === "number") {
    if (!Number.isFinite(value)) {
      throw new Error(`${path} must contain only finite numbers`);
    }
    return;
  }
  if (Array.isArray(value)) {
    value.forEach((entry, index) => requireJson(entry, `${path}[${index}]`));
    return;
  }
  if (isObject(value)) {
    for (const [key, entry] of Object.entries(value)) {
      requireJson(entry, `${path}.${key}`);
    }
    return;
  }
  throw new Error(`${path} must be JSON-compatible`);
}

function frozenJson(value: JsonValue): JsonValue {
  if (Array.isArray(value)) {
    return Object.freeze(value.map(frozenJson));
  }
  if (isObject(value)) {
    return Object.freeze(
      Object.fromEntries(
        Object.entries(value).map(([key, entry]) => [
          key,
          frozenJson(entry as JsonValue),
        ]),
      ),
    );
  }
  return value;
}

export function fixtureKey(descriptor: FixtureDescriptor): string {
  const response = descriptor.response;
  const stream = response?.stream;
  return JSON.stringify({
    source: descriptor.source,
    ...(response
      ? {
          response: {
            ...(response.contentType !== undefined
              ? { contentType: response.contentType }
              : {}),
            ...(stream
              ? {
                  stream: {
                    chunkSize: stream.chunkSize,
                    ...(stream.intervalMs !== undefined
                      ? { intervalMs: stream.intervalMs }
                      : {}),
                  },
                }
              : {}),
          },
        }
      : {}),
  });
}

function fixtureResponse(value: unknown): FixtureResponse | undefined {
  if (value === undefined) return undefined;
  if (!isObject(value)) {
    throw new Error("fixture response must be an object");
  }
  const contentType = value.contentType;
  if (
    contentType !== undefined &&
    (typeof contentType !== "string" || contentType.trim().length === 0)
  ) {
    throw new Error("fixture response contentType must be a non-empty string");
  }
  const stream = value.stream;
  if (stream !== undefined && !isObject(stream)) {
    throw new Error("fixture response stream must be an object");
  }
  const chunkSize = stream?.chunkSize;
  const intervalMs = stream?.intervalMs ?? 0;
  if (
    stream &&
    (!Number.isSafeInteger(chunkSize) ||
      (chunkSize as number) <= 0 ||
      !Number.isSafeInteger(intervalMs) ||
      (intervalMs as number) < 0)
  ) {
    throw new Error(
      "fixture stream chunkSize must be positive and intervalMs must be non-negative",
    );
  }
  return Object.freeze({
    ...(contentType ? { contentType } : {}),
    ...(stream
      ? {
          stream: Object.freeze({
            chunkSize: chunkSize as number,
            ...("intervalMs" in stream
              ? { intervalMs: intervalMs as number }
              : {}),
          }),
        }
      : {}),
  });
}

function operation(value: unknown): BenchmarkOperation {
  if (!isObject(value)) {
    throw new Error("benchmark operation must be an object");
  }
  return {
    case_id: requireIdentifier("benchmark operation case_id", value.case_id),
  };
}

function browserOrigin(): string {
  const origin = globalThis.location?.origin;
  if (!origin) {
    throw new Error("benchmark fixtures require a browser HTTP origin");
  }
  const parsed = new URL(origin);
  if (!["http:", "https:"].includes(parsed.protocol)) {
    throw new Error("benchmark origin must use HTTP");
  }
  return parsed.origin;
}

export function fixture(
  source: string,
  options: FixtureOptions = {},
): BrowserFixture {
  if (typeof source !== "string" || source.trim().length === 0) {
    throw new Error("fixture source must be a non-empty string");
  }
  const origin = browserOrigin();
  const response = fixtureResponse(options?.response);
  const descriptor: FixtureDescriptor = {
    source,
    ...(response ? { response } : {}),
  };
  const key = fixtureKey(descriptor);
  fixtureDescriptors.set(key, Object.freeze(descriptor));
  const url = new URL("/__bperf/fixture", origin);
  url.searchParams.set("descriptor", key);
  return Object.freeze({ url });
}

export function exact(value: JsonValue): ExactExpectation {
  requireJson(value, "exact expectation");
  const expectation = Object.freeze({
    kind: "exact",
    value: frozenJson(value),
  } satisfies ExactExpectation);
  exactExpectations.add(expectation);
  return expectation;
}

export function defineBrowserBenchmark<const States extends readonly unknown[]>(
  definition: BrowserBenchmarkDefinition<States>,
): BrowserBenchmarkDefinition<States> {
  const benchmarkId = requireIdentifier("benchmark id", definition?.id);
  const engines = engineScope(definition?.engines);
  const gpu = gpuRequirement(definition?.gpu);
  if (!Array.isArray(definition?.cases) || definition.cases.length === 0) {
    throw new Error("browser benchmark must define at least one case");
  }

  const cases = new Map<string, BrowserBenchmarkCase<unknown>>();
  for (const candidate of definition.cases) {
    const id = requireIdentifier("benchmark case id", candidate?.id);
    if (cases.has(id)) {
      throw new Error(`duplicate benchmark case ${JSON.stringify(id)}`);
    }
    if (typeof candidate.measure !== "function") {
      throw new Error(
        `benchmark case ${JSON.stringify(id)} requires measure()`,
      );
    }
    if (
      candidate.setup !== undefined &&
      typeof candidate.setup !== "function"
    ) {
      throw new Error(
        `benchmark case ${JSON.stringify(id)} setup must be a function`,
      );
    }
    if (
      candidate.settle !== undefined &&
      typeof candidate.settle !== "function"
    ) {
      throw new Error(
        `benchmark case ${JSON.stringify(id)} settle must be a function`,
      );
    }
    if (
      candidate.expect?.kind !== "exact" ||
      !exactExpectations.has(candidate.expect)
    ) {
      throw new Error(
        `benchmark case ${JSON.stringify(id)} requires expect: exact(...)`,
      );
    }
    requireJson(candidate.expect.value, `benchmark case ${id} expectation`);
    Object.freeze(candidate);
    cases.set(id, candidate);
  }

  Object.freeze(definition.cases);
  if (definition.engines) Object.freeze(definition.engines);
  Object.freeze(definition);
  installPageAdapter(definition, {
    id: benchmarkId,
    engines,
    gpu,
    cases,
    fixtures: [...fixtureDescriptors.values()],
  });
  fixtureDescriptors.clear();
  return definition;
}

function installPageAdapter(
  definition: object,
  registered: RegisteredBenchmark,
): void {
  globalThis.__bperfDefinition = definition;
  const gpuLoss = registered.gpu ? watchGpuLoss() : undefined;
  const prepared = new Map<string, unknown>();
  const measured = new Set<string>();
  globalThis.__bperfDescription = {
    id: registered.id,
    engines: registered.engines,
    gpu: registered.gpu,
    cases: [...registered.cases].map(([id, candidate]) => ({
      id,
      expectation: candidate.expect,
    })),
    fixtures: registered.fixtures,
  };
  globalThis.__bperf = {
    async prepare(values) {
      for (const value of values) {
        const { case_id: caseId } = operation(value);
        const candidate = registered.cases.get(caseId);
        if (!candidate) {
          throw new Error(`unknown benchmark case ${JSON.stringify(caseId)}`);
        }
        if (!prepared.has(caseId)) {
          prepared.set(caseId, await candidate.setup?.());
        }
      }
    },

    async run(value) {
      const { case_id: caseId } = operation(value);
      const candidate = registered.cases.get(caseId);
      if (!candidate || !prepared.has(caseId)) {
        throw new Error(
          `benchmark case ${JSON.stringify(caseId)} was not prepared`,
        );
      }
      const result = await candidate.measure(prepared.get(caseId));
      const lost = gpuLoss?.();
      if (lost) {
        throw new Error(`GPU ${lost} was lost during benchmark case ${caseId}`);
      }
      requireJson(result, `benchmark case ${caseId} result`);
      measured.add(caseId);
      return result;
    },

    async settle() {
      for (const caseId of measured) {
        const candidate = registered.cases.get(caseId);
        await candidate?.settle?.(prepared.get(caseId));
      }
    },
  };
}

// A lost context or device silently turns later GPU calls into no-ops, so a
// trial that loses one would look faster without doing the work. The returned
// function reports the first loss since the page loaded.
function watchGpuLoss(): () => string | undefined {
  let lost: string | undefined;
  const observe = (target: EventTarget) =>
    target.addEventListener("webglcontextlost", () => {
      lost ??= "context";
    });
  for (const Canvas of [globalThis.HTMLCanvasElement, globalThis.OffscreenCanvas]) {
    const prototype = Canvas?.prototype as
      | { getContext(this: EventTarget, ...args: unknown[]): unknown }
      | undefined;
    if (!prototype) continue;
    const getContext = prototype.getContext;
    prototype.getContext = function (this: EventTarget, ...args: unknown[]) {
      const context = getContext.apply(this, args);
      if (typeof args[0] === "string" && args[0].startsWith("webgl")) {
        observe(this);
      }
      return context;
    };
  }
  const adapter = (globalThis as { GPUAdapter?: { prototype: GpuAdapterLike } })
    .GPUAdapter?.prototype;
  if (adapter) {
    const requestDevice = adapter.requestDevice;
    adapter.requestDevice = async function (this: GpuAdapterLike, ...args: unknown[]) {
      const device = await requestDevice.apply(this, args);
      device.lost.then((info) => {
        if (info.reason !== "destroyed") lost ??= "device";
      });
      return device;
    };
  }
  return () => lost;
}

interface GpuAdapterLike {
  requestDevice(...args: unknown[]): Promise<GpuDeviceLike>;
}

interface GpuDeviceLike {
  readonly lost: Promise<{ reason: string }>;
  readonly queue: {
    submit(commands: unknown[]): void;
  };
  createBuffer(descriptor: { size: number; usage: number }): GpuBufferLike;
  createCommandEncoder(): {
    copyBufferToBuffer(
      source: unknown,
      sourceOffset: number,
      destination: unknown,
      destinationOffset: number,
      size: number,
    ): void;
    finish(): unknown;
  };
}

interface GpuBufferLike {
  readonly size: number;
  mapAsync(mode: number): Promise<void>;
  getMappedRange(): ArrayBuffer;
  unmap(): void;
  destroy(): void;
}

/**
 * Returns a 64-bit digest of `bytes` as 16 hex digits. It is a correctness
 * fingerprint for `exact()`, not a cryptographic hash.
 */
export function digestBytes(bytes: ArrayBufferView | ArrayBuffer): string {
  const view = bytes instanceof ArrayBuffer
    ? new Uint8Array(bytes)
    : new Uint8Array(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  let low = 0x811c9dc5;
  let high = 0x01000193;
  for (let index = 0; index < view.length; index += 1) {
    low = Math.imul(low ^ view[index], 0x01000193) >>> 0;
    high = Math.imul(high ^ view[index] ^ (low >>> 7), 0x5bd1e995) >>> 0;
  }
  return high.toString(16).padStart(8, "0") + low.toString(16).padStart(8, "0");
}

export interface PixelRegion {
  x: number;
  y: number;
  width: number;
  height: number;
}

/**
 * Reads back RGBA pixels from the current WebGL framebuffer and digests them.
 * `readPixels` waits for every queued draw, so a case that returns this value
 * measures completed GPU work. Defaults to the whole drawing buffer; a small
 * region keeps the hashing cost out of throughput measurements.
 */
export function readPixelsDigest(
  gl: WebGLRenderingContext | WebGL2RenderingContext,
  region: PixelRegion = {
    x: 0,
    y: 0,
    width: gl.drawingBufferWidth,
    height: gl.drawingBufferHeight,
  },
): string {
  const pixels = new Uint8Array(region.width * region.height * 4);
  gl.readPixels(
    region.x,
    region.y,
    region.width,
    region.height,
    gl.RGBA,
    gl.UNSIGNED_BYTE,
    pixels,
  );
  if (gl.isContextLost()) {
    throw new Error("WebGL context was lost before readback");
  }
  return digestBytes(pixels);
}

/**
 * Copies a WebGPU buffer to a mappable staging buffer, waits for the queue to
 * reach the copy, and digests the bytes. The source buffer needs
 * `GPUBufferUsage.COPY_SRC`.
 */
export async function readBufferDigest(
  device: GpuDeviceLike,
  buffer: GpuBufferLike,
  size: number = buffer.size,
): Promise<string> {
  const usage = globalThis as unknown as {
    GPUBufferUsage: { MAP_READ: number; COPY_DST: number };
    GPUMapMode: { READ: number };
  };
  const staging = device.createBuffer({
    size,
    usage: usage.GPUBufferUsage.MAP_READ | usage.GPUBufferUsage.COPY_DST,
  });
  try {
    const encoder = device.createCommandEncoder();
    encoder.copyBufferToBuffer(buffer, 0, staging, 0, size);
    device.queue.submit([encoder.finish()]);
    await staging.mapAsync(usage.GPUMapMode.READ);
    const digest = digestBytes(new Uint8Array(staging.getMappedRange()));
    staging.unmap();
    return digest;
  } finally {
    staging.destroy();
  }
}
