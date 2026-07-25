import {
  positiveWeights,
  SpeedscopeBuilder,
} from "../speedscope.ts";
import { firefoxHeapSnapshotLiveBytes } from "./firefox-heap-snapshot.ts";

type GeckoTableRow = Array<number | null>;

interface GeckoProfile {
  meta?: {
    interval?: number;
    processType?: number | string;
  };
  threads?: GeckoThread[];
  processes?: GeckoProfile[];
}

interface GeckoThread {
  name: string;
  pid: number | string;
  tid: number | string;
  samples: {
    schema: { stack: number; time: number };
    data: GeckoTableRow[];
  };
  stackTable: {
    schema: { prefix: number; frame: number };
    data: GeckoTableRow[];
  };
  frameTable: {
    schema: { location: number };
    data: GeckoTableRow[];
  };
  stringTable: string[];
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function parseProfile(source: string): GeckoProfile {
  const value: unknown = JSON.parse(source);
  if (
    !isObject(value) ||
    !Array.isArray(value.threads) ||
    !Array.isArray(value.processes)
  ) {
    throw new Error("Firefox emitted an invalid Gecko Profiler document");
  }
  return value as unknown as GeckoProfile;
}

function stackContainsUrl(
  thread: GeckoThread,
  stackIndex: number,
  targetUrl: string,
  cache: Map<number, boolean>,
): boolean {
  const cached = cache.get(stackIndex);
  if (cached !== undefined) return cached;
  const stackRow = thread.stackTable.data[stackIndex];
  if (!stackRow) return false;

  const prefix = stackRow[thread.stackTable.schema.prefix];
  const frameIndex = stackRow[thread.stackTable.schema.frame];
  const frameRow =
    typeof frameIndex === "number"
      ? thread.frameTable.data[frameIndex]
      : undefined;
  const locationIndex = frameRow?.[thread.frameTable.schema.location];
  const location =
    typeof locationIndex === "number"
      ? thread.stringTable[locationIndex]
      : undefined;
  const result =
    Boolean(location?.includes(targetUrl)) ||
    (typeof prefix === "number" &&
      stackContainsUrl(thread, prefix, targetUrl, cache));
  cache.set(stackIndex, result);
  return result;
}

function cpuActiveMilliseconds(
  rootProfile: GeckoProfile,
  targetUrl: string,
): number {
  const rootInterval = rootProfile.meta?.interval ?? 1;

  function processDuration(profile: GeckoProfile): number {
    const interval = profile.meta?.interval ?? rootInterval;
    let duration = 0;
    for (const thread of profile.threads ?? []) {
      const stackColumn = thread.samples.schema.stack;
      const timeColumn = thread.samples.schema.time;
      const samples = thread.samples.data.flatMap((row) => {
        const stackIndex = row[stackColumn];
        const time = row[timeColumn];
        return typeof stackIndex === "number" &&
          typeof time === "number" &&
          Number.isFinite(time)
          ? [{ stackIndex, time }]
          : [];
      });
      if (samples.length > 0) {
        const weights = positiveWeights(
          samples.map(({ time }) => time),
          interval,
        );
        const targetCache = new Map<number, boolean>();
        duration += samples.reduce(
          (sum, { stackIndex }, index) =>
            sum +
            (stackContainsUrl(
              thread,
              stackIndex,
              targetUrl,
              targetCache,
            )
              ? weights[index]
              : 0),
          0,
        );
      }
    }
    for (const child of profile.processes ?? []) {
      duration += processDuration(child);
    }
    return duration;
  }

  const duration = processDuration(rootProfile);
  if (!(duration > 0)) {
    throw new Error("Firefox CPU profile has no positive sample duration");
  }
  return duration;
}

function writeFlamegraph(
  rootProfile: GeckoProfile,
  filePath: string,
  targetUrl?: string,
): void {
  const builder = new SpeedscopeBuilder("Firefox CPU");
  const rootInterval = rootProfile.meta?.interval ?? 1;

  function addProcess(
    profile: GeckoProfile,
    processPath: string[],
  ): void {
    const processName =
      profile.meta?.processType === 0
        ? "Parent"
        : profile.meta?.processType ?? "Content";
    const label = [...processPath, String(processName)].join(" / ");

    for (const thread of profile.threads ?? []) {
      const stackColumn = thread.samples.schema.stack;
      const timeColumn = thread.samples.schema.time;
      const stackPrefixColumn = thread.stackTable.schema.prefix;
      const stackFrameColumn = thread.stackTable.schema.frame;
      const frameLocationColumn = thread.frameTable.schema.location;
      const stackCache = new Map<number, number[]>();
      const targetCache = new Map<number, boolean>();

      function stackFor(stackIndex: number): number[] {
        const cached = stackCache.get(stackIndex);
        if (cached) return cached;

        const stackRow = thread.stackTable.data[stackIndex];
        if (!stackRow) return [];
        const prefix = stackRow[stackPrefixColumn];
        const frames =
          typeof prefix === "number" ? [...stackFor(prefix)] : [];
        const frameIndex = stackRow[stackFrameColumn];
        if (typeof frameIndex !== "number") return frames;
        const frameRow = thread.frameTable.data[frameIndex];
        if (!frameRow) return frames;
        const locationIndex = frameRow[frameLocationColumn];
        const location =
          typeof locationIndex === "number"
            ? thread.stringTable[locationIndex]
            : undefined;
        frames.push(builder.frame({ name: location ?? "(unknown)" }));
        stackCache.set(stackIndex, frames);
        return frames;
      }

      const samples = thread.samples.data.flatMap((row) => {
        const stackIndex = row[stackColumn];
        const time = row[timeColumn];
        return typeof stackIndex === "number" &&
          typeof time === "number" &&
          Number.isFinite(time) &&
          (!targetUrl ||
            stackContainsUrl(
              thread,
              stackIndex,
              targetUrl,
              targetCache,
            ))
          ? [{ stackIndex, time }]
          : [];
      });
      if (samples.length === 0) continue;

      const timestamps = samples.map(({ time }) => time);
      builder.sampledProfile({
        name: `${label} / ${thread.name} (${thread.pid}:${thread.tid})`,
        unit: "milliseconds",
        startValue: timestamps[0],
        samples: samples.map(({ stackIndex }) => stackFor(stackIndex)),
        weights: positiveWeights(
          timestamps,
          profile.meta?.interval ?? rootInterval,
        ),
      });
    }

    for (const child of profile.processes ?? []) {
      addProcess(child, [...processPath, String(processName)]);
    }
  }

  addProcess(rootProfile, []);
  builder.write(filePath);
}

export const firefoxArtifactFormat = {
  parseProfile,
  cpuActiveMilliseconds,
  liveHeapBytes: firefoxHeapSnapshotLiveBytes,
  writeFlamegraph,
} as const;
