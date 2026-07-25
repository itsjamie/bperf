import type { Page } from "playwright";

import { RUNTIME_ANCHOR } from "./contract.ts";
import type { RuntimeAnchorEvidence } from "./contract.ts";

const WARMUP_SAMPLES = 4;
const BASE_ROUNDS = 512;
const TARGET_SAMPLE_MS = 75;

export async function measureRuntimeAnchor(
  page: Page,
): Promise<RuntimeAnchorEvidence> {
  return page.evaluate(
    ({
      baseRounds,
      maxBatchSize,
      measuredSamples,
      targetSampleMs,
      warmupSamples,
      workload,
    }) => {
      const values = new Uint32Array(4_096);
      for (let index = 0; index < values.length; index += 1) {
        values[index] = Math.imul(index + 1, 2_654_435_761) >>> 0;
      }

      function run(rounds: number): number {
        let checksum = 2_166_136_261;
        for (let round = 0; round < rounds; round += 1) {
          for (let index = 0; index < values.length; index += 1) {
            const value = values[(index + round) & (values.length - 1)];
            checksum = Math.imul(checksum ^ value, 16_777_619) >>> 0;
          }
        }
        return checksum;
      }

      run(baseRounds);
      const calibrationStarted = performance.now();
      run(baseRounds);
      const calibrationMs = performance.now() - calibrationStarted;
      const batchSize = Math.max(
        1,
        Math.min(maxBatchSize, Math.ceil(targetSampleMs / calibrationMs)),
      );
      const rounds = baseRounds * batchSize;

      for (let index = 0; index < warmupSamples; index += 1) run(rounds);

      const wallMs: number[] = [];
      let checksum = 0;
      for (let index = 0; index < measuredSamples; index += 1) {
        const started = performance.now();
        const result = run(rounds);
        const elapsed = (performance.now() - started) / batchSize;
        if (index > 0 && result !== checksum) {
          throw new Error("runtime anchor produced an unstable checksum");
        }
        checksum = result;
        wallMs.push(elapsed);
      }
      return {
        workload,
        wall_ms: wallMs,
        batch_size: batchSize,
        checksum,
      };
    },
    {
      baseRounds: BASE_ROUNDS,
      maxBatchSize: RUNTIME_ANCHOR.maxBatchSize,
      measuredSamples: RUNTIME_ANCHOR.samples,
      targetSampleMs: TARGET_SAMPLE_MS,
      warmupSamples: WARMUP_SAMPLES,
      workload: RUNTIME_ANCHOR.workload,
    },
  );
}
