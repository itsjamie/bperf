import type {
  EngineId,
  TrialCapture,
  TrialEngineAdapter,
  TrialEvidence,
  TrialLane,
  TrialRequest,
} from "./contract.ts";
import { chromiumTrialAdapter } from "./engines/chromium.ts";
import { firefoxTrialAdapter } from "./engines/firefox.ts";
import { webkitTrialAdapter } from "./engines/webkit.ts";
import { runtimeInfo } from "./playwright-runtime.ts";

const defaultAdapters = {
  chromium: chromiumTrialAdapter,
  firefox: firefoxTrialAdapter,
  webkit: webkitTrialAdapter,
} satisfies Record<EngineId, TrialEngineAdapter>;

class RetainedLane<Capture> {
  #lane: Promise<TrialLane<Capture>> | undefined;
  readonly #open: () => Promise<TrialLane<Capture>>;

  constructor(open: () => Promise<TrialLane<Capture>>) {
    this.#open = open;
  }

  async capture(request: TrialRequest): Promise<Capture> {
    let lane: TrialLane<Capture>;
    try {
      this.#lane ??= this.#open();
      lane = await this.#lane;
    } catch (error) {
      this.#lane = undefined;
      throw error;
    }

    try {
      return await lane.capture(request);
    } catch (error) {
      this.#lane = undefined;
      try {
        await lane.close();
      } catch (closeError) {
        throw new AggregateError(
          [error, closeError],
          "trial capture failed and its browser lane could not close",
        );
      }
      throw error;
    }
  }

  async close(): Promise<void> {
    const pending = this.#lane;
    this.#lane = undefined;
    if (pending) {
      await (await pending).close();
    }
  }
}

export class BrowserTrialLab {
  readonly #lanes = new Map<EngineId, RetainedLane<TrialCapture>>();
  readonly #adapters: Record<EngineId, TrialEngineAdapter>;
  #closed = false;

  constructor(
    adapters: Record<EngineId, TrialEngineAdapter> = defaultAdapters,
  ) {
    this.#adapters = adapters;
  }

  async measureBrowserTrial(
    engine: EngineId,
    request: TrialRequest,
  ): Promise<TrialEvidence> {
    if (this.#closed) {
      throw new Error("browser trial lab is closed");
    }
    const started = performance.now();
    const capture = await this.#engineLane(engine).capture(request);
    const captureElapsedMs = Math.max(
      0.001,
      performance.now() - started,
    );

    return {
      engine,
      runtime: runtimeInfo(),
      browser: capture.browser,
      capture_elapsed_ms: captureElapsedMs,
      workload: capture.workload,
      metrics: {
        "workload.wall_ms": capture.workload.workload_wall_ms,
        "variant.call_wall_ms":
          capture.workload.variant_call_wall_ms,
        "browser.cpu_profile.active_ms":
          capture.cpu_active_ms,
        "browser.js_heap.live_bytes":
          capture.js_heap_live_bytes,
        "bperf.capture.elapsed_ms": captureElapsedMs,
        "bperf.batch_size": capture.workload.batch_size,
      },
      artifacts: capture.artifacts,
    };
  }

  async close(): Promise<void> {
    if (this.#closed) return;
    this.#closed = true;
    const lanes = [...this.#lanes.values()];
    this.#lanes.clear();
    const results = await Promise.allSettled(
      lanes.map((lane) => lane.close()),
    );
    const failures = results
      .filter((result) => result.status === "rejected")
      .map((result) => result.reason);
    if (failures.length > 0) {
      throw new AggregateError(
        failures,
        "one or more browser lanes failed to close",
      );
    }
  }

  #engineLane(engine: EngineId): RetainedLane<TrialCapture> {
    let lane = this.#lanes.get(engine);
    if (!lane) {
      lane = new RetainedLane(
        this.#adapters[engine].openTrialLane,
      );
      this.#lanes.set(engine, lane);
    }
    return lane;
  }
}
