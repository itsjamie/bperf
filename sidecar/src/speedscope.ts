import fs from "node:fs";

type SpeedscopeUnit =
  | "none"
  | "nanoseconds"
  | "microseconds"
  | "milliseconds"
  | "seconds"
  | "bytes";

interface FrameInput {
  name: string;
  file?: string;
  line?: number;
  col?: number;
}

interface SpeedscopeFrame {
  name: string;
  file?: string;
  line?: number;
  col?: number;
}

interface SampledProfileInput {
  name: string;
  unit: SpeedscopeUnit;
  samples: number[][];
  weights: number[];
  startValue?: number;
}

interface SampledProfile {
  type: "sampled";
  name: string;
  unit: SpeedscopeUnit;
  startValue: number;
  endValue: number;
  samples: number[][];
  weights: number[];
}

interface SpeedscopeDocument {
  $schema: "https://www.speedscope.app/file-format-schema.json";
  name: string;
  exporter: "bperf Playwright sidecar";
  activeProfileIndex: 0;
  shared: { frames: SpeedscopeFrame[] };
  profiles: SampledProfile[];
}

export class SpeedscopeBuilder {
  readonly #name: string;
  readonly #frames: SpeedscopeFrame[] = [];
  readonly #frameIndexes = new Map<string, number>();
  readonly #profiles: SampledProfile[] = [];

  constructor(name: string) {
    this.#name = name;
  }

  frame({ name, file, line, col }: FrameInput): number {
    const frame: SpeedscopeFrame = {
      name: name || "(anonymous)",
      ...(file ? { file } : {}),
      ...(Number.isFinite(line) && Number(line) >= 0 ? { line } : {}),
      ...(Number.isFinite(col) && Number(col) >= 0 ? { col } : {}),
    };
    const key = JSON.stringify(frame);
    const existing = this.#frameIndexes.get(key);
    if (existing !== undefined) return existing;

    const index = this.#frames.length;
    this.#frameIndexes.set(key, index);
    this.#frames.push(frame);
    return index;
  }

  sampledProfile({
    name,
    unit,
    samples,
    weights,
    startValue = 0,
  }: SampledProfileInput): void {
    if (samples.length === 0 || samples.length !== weights.length) {
      throw new Error(`Invalid Speedscope sample data for ${name}`);
    }
    if (samples.some((stack) => stack.length === 0)) {
      throw new Error(`Empty Speedscope stack in ${name}`);
    }
    const duration = weights.reduce((sum, value) => sum + value, 0);
    if (!(duration > 0)) {
      throw new Error(`Non-positive Speedscope duration for ${name}`);
    }
    this.#profiles.push({
      type: "sampled",
      name,
      unit,
      startValue,
      endValue: startValue + duration,
      samples,
      weights,
    });
  }

  document(): SpeedscopeDocument {
    if (this.#frames.length === 0 || this.#profiles.length === 0) {
      throw new Error(`No Speedscope data for ${this.#name}`);
    }
    return {
      $schema: "https://www.speedscope.app/file-format-schema.json",
      name: this.#name,
      exporter: "bperf Playwright sidecar",
      activeProfileIndex: 0,
      shared: { frames: this.#frames },
      profiles: this.#profiles,
    };
  }

  write(filePath: string): void {
    fs.writeFileSync(filePath, JSON.stringify(this.document()));
  }
}

export function positiveWeights(
  timestamps: number[],
  fallback: number,
): number[] {
  if (!(fallback > 0)) {
    throw new Error("Speedscope fallback weight must be positive");
  }

  const positiveDeltas: number[] = [];
  for (let index = 0; index < timestamps.length - 1; index += 1) {
    const delta = timestamps[index + 1] - timestamps[index];
    if (delta > 0) positiveDeltas.push(delta);
  }
  positiveDeltas.sort((left, right) => left - right);
  const typical =
    positiveDeltas[Math.floor(positiveDeltas.length / 2)] ?? fallback;
  return timestamps.map((timestamp, index) => {
    const next = timestamps[index + 1];
    return next !== undefined && next > timestamp
      ? next - timestamp
      : typical;
  });
}
