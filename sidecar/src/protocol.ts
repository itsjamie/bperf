import { captureEngine, isSupportedEngine } from "./capture.ts";
import type {
  BrowserTrialConfig,
  EngineId,
  TrialRequest,
} from "./contract.ts";
import { BrowserTrialLab } from "./trial.ts";
import { isAllowedAdapterUrl } from "./trial-workload.ts";

export const PROTOCOL_VERSION = 9;

interface WireError {
  code: string;
  message: string;
  stack?: string;
}

interface SuccessResponse {
  protocol_version: typeof PROTOCOL_VERSION;
  id: number;
  ok: true;
  result: unknown;
}

interface ErrorResponse {
  protocol_version: typeof PROTOCOL_VERSION;
  id: number;
  ok: false;
  error: WireError;
}

export type WireResponse = SuccessResponse | ErrorResponse;

export interface ProtocolResult {
  response: WireResponse;
  terminate: boolean;
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function requestId(value: unknown): number {
  return typeof value === "number" &&
    Number.isSafeInteger(value) &&
    value >= 0
    ? value
    : 0;
}

function asError(value: unknown): Error {
  return value instanceof Error ? value : new Error(String(value));
}

function nonEmptyString(value: unknown): value is string {
  return typeof value === "string" && value.length > 0;
}

function positiveInteger(value: unknown): value is number {
  return Number.isSafeInteger(value) && Number(value) > 0;
}

function positiveNumber(value: unknown): value is number {
  return typeof value === "number" &&
    Number.isFinite(value) &&
    value > 0;
}

function trialRequest(params: unknown): {
  engine: EngineId;
  request: TrialRequest;
} {
  if (!isObject(params)) {
    throw new Error("measure_trial requires parameters");
  }
  const browser = params.browser;
  const viewport = isObject(browser) ? browser.viewport : undefined;
  const colorScheme = isObject(browser)
    ? browser.color_scheme
    : undefined;
  if (
    !isSupportedEngine(params.engine) ||
    !nonEmptyString(params.artifact_dir) ||
    !nonEmptyString(params.target_url) ||
    !Array.isArray(params.operations) ||
    params.operations.length === 0 ||
    !positiveInteger(params.batch_size) ||
    !positiveInteger(params.batch_max_size) ||
    params.batch_max_size < params.batch_size ||
    (params.batch_target_ms !== null &&
      params.batch_target_ms !== undefined &&
      !positiveNumber(params.batch_target_ms)) ||
    !isAllowedAdapterUrl(params.target_url) ||
    !isObject(browser) ||
    !isObject(viewport) ||
    !positiveInteger(viewport.width) ||
    !positiveInteger(viewport.height) ||
    !nonEmptyString(browser.locale) ||
    !nonEmptyString(browser.timezone_id) ||
    !["light", "dark", "no-preference"].includes(String(colorScheme))
  ) {
    throw new Error(
      "measure_trial requires an engine, artifact_dir, target_url, operations, a valid batch plan, and complete browser configuration",
    );
  }

  const config: BrowserTrialConfig = {
    viewport: {
      width: viewport.width,
      height: viewport.height,
    },
    locale: browser.locale,
    timezone_id: browser.timezone_id,
    color_scheme: colorScheme as BrowserTrialConfig["color_scheme"],
  };
  return {
    engine: params.engine,
    request: {
      targetUrl: params.target_url,
      operations: params.operations,
      artifactDirectory: params.artifact_dir,
      browser: config,
      batchSize: params.batch_size,
      ...(typeof params.batch_target_ms === "number"
        ? { batchTargetMs: params.batch_target_ms }
        : {}),
      batchMaxSize: params.batch_max_size,
    },
  };
}

function protocolError(
  id: number,
  code: string,
  value: unknown,
): ErrorResponse {
  const error = asError(value);
  return {
    protocol_version: PROTOCOL_VERSION,
    id,
    ok: false,
    error: {
      code,
      message: error.message,
      ...(error.stack ? { stack: error.stack } : {}),
    },
  };
}

export function invalidRequest(value: unknown): WireResponse {
  return protocolError(0, "invalid_request", value);
}

type BrowserTrials = Pick<
  BrowserTrialLab,
  "measureBrowserTrial" | "close"
>;

async function dispatchRequest(
  browserTrials: BrowserTrials,
  value: unknown,
): Promise<ProtocolResult> {
  if (!isObject(value)) {
    return {
      response: protocolError(
        0,
        "invalid_request",
        new Error("Sidecar request must be a JSON object"),
      ),
      terminate: false,
    };
  }

  const id = requestId(value.id);
  if (value.protocol_version !== PROTOCOL_VERSION) {
    return {
      response: protocolError(
        id,
        "protocol_version_mismatch",
        new Error(
          `Expected protocol ${PROTOCOL_VERSION}, received ${String(
            value.protocol_version,
          )}`,
        ),
      ),
      terminate: false,
    };
  }
  if (id === 0 && value.id !== 0) {
    return {
      response: protocolError(
        0,
        "invalid_request",
        new Error("Sidecar request id must be a non-negative safe integer"),
      ),
      terminate: false,
    };
  }

  if (value.method === "shutdown") {
    try {
      await browserTrials.close();
    } catch (error) {
      return {
        response: protocolError(id, "shutdown_failed", error),
        terminate: true,
      };
    }
    return {
      response: {
        protocol_version: PROTOCOL_VERSION,
        id,
        ok: true,
        result: { shutdown: true },
      },
      terminate: true,
    };
  }
  if (value.method === "measure_trial") {
    let trial;
    try {
      trial = trialRequest(value.params);
    } catch (error) {
      return {
        response: protocolError(id, "invalid_params", error),
        terminate: false,
      };
    }
    try {
      return {
        response: {
          protocol_version: PROTOCOL_VERSION,
          id,
          ok: true,
          result: await browserTrials.measureBrowserTrial(
            trial.engine,
            trial.request,
          ),
        },
        terminate: false,
      };
    } catch (error) {
      return {
        response: protocolError(id, "trial_failed", error),
        terminate: false,
      };
    }
  }
  if (value.method !== "doctor") {
    return {
      response: protocolError(
        id,
        "unknown_method",
        new Error(`Unknown sidecar method: ${String(value.method)}`),
      ),
      terminate: false,
    };
  }

  const params = isObject(value.params) ? value.params : undefined;
  const engine = params?.engine;
  const artifactDirectory = params?.artifact_dir;
  if (
    !isSupportedEngine(engine) ||
    typeof artifactDirectory !== "string" ||
    artifactDirectory.length === 0
  ) {
    return {
      response: protocolError(
        id,
        "invalid_params",
        new Error("doctor requires a valid engine and artifact_dir"),
      ),
      terminate: false,
    };
  }

  try {
    return {
      response: {
        protocol_version: PROTOCOL_VERSION,
        id,
        ok: true,
        result: await captureEngine(engine, artifactDirectory),
      },
      terminate: false,
    };
  } catch (error) {
    return {
      response: protocolError(id, "capture_failed", error),
      terminate: false,
    };
  }
}

export function createProtocolHandler(
  browserTrials: BrowserTrials = new BrowserTrialLab(),
) {
  return (value: unknown) => dispatchRequest(browserTrials, value);
}

export const handleRequest = createProtocolHandler();
