// Values crossing the browser/host boundary must remain JSON-compatible.
export type JsonValue =
  | null
  | boolean
  | number
  | string
  | readonly JsonValue[]
  | { readonly [key: string]: JsonValue };

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
  cases: Map<string, BrowserBenchmarkCase<unknown>>;
  fixtures: FixtureDescriptor[];
}

declare global {
  var __bperf: BenchmarkPageAdapter | undefined;
  var __bperfDescription: BrowserBenchmarkDescription | undefined;
  var __bperfDefinition: object | undefined;
}

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
  Object.freeze(definition);
  installPageAdapter(definition, {
    id: benchmarkId,
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
  const prepared = new Map<string, unknown>();
  const measured = new Set<string>();
  globalThis.__bperfDescription = {
    id: registered.id,
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
