(() => {
  "use strict";

  const VERSION = 2;
  const RUNTIME_ANCHOR = Object.freeze({
    workload: "javascript_cpu_v1",
    samples: 31,
    maxBatchSize: 64,
    warmupSamples: 4,
    baseRounds: 512,
    targetSampleMs: 75,
  });

  function adapter() {
    const target = globalThis.__bperf;
    if (typeof target?.run !== "function") {
      throw new Error("Page benchmark adapter has no run(operation) method");
    }
    return target;
  }

  async function prepare(operations) {
    await adapter().prepare?.(operations);
  }

  async function execute(operations, batchSize = 1) {
    if (!Number.isSafeInteger(batchSize) || batchSize <= 0) {
      throw new Error("benchmark batch size must be a positive safe integer");
    }

    // Short calls are grouped before clock reads so sub-millisecond timer
    // quantization does not dominate variant-call timing.
    const measurementGroupSize = 32;
    const target = adapter();
    const started = performance.now();
    let variantCallWallMs = 0;
    let result;
    let encodedResult;
    for (
      let groupStart = 0;
      groupStart < batchSize;
      groupStart += measurementGroupSize
    ) {
      const group = [];
      const groupEnd = Math.min(
        batchSize,
        groupStart + measurementGroupSize,
      );
      const callsStarted = performance.now();
      for (
        let repetition = groupStart;
        repetition < groupEnd;
        repetition += 1
      ) {
        const current = [];
        for (const operation of operations) {
          current.push(await target.run(operation));
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
      workload_wall_ms: batchWallMs / batchSize,
      variant_call_wall_ms: variantCallWallMs / batchSize,
      batch_wall_ms: batchWallMs,
      batch_size: batchSize,
      operation_count: operations.length,
      result: result ?? [],
    };
  }

  async function selectBatchSize(
    operations,
    initialSize,
    targetMs,
    maximumSize,
  ) {
    if (targetMs === undefined || targetMs === null) return initialSize;

    let batchSize = initialSize;
    let confirmedSize;
    while (true) {
      const workload = await execute(operations, batchSize);
      if (workload.batch_wall_ms >= targetMs) {
        if (confirmedSize === batchSize || batchSize === maximumSize) {
          return batchSize;
        }
        confirmedSize = batchSize;
        continue;
      }
      if (batchSize === maximumSize) {
        return batchSize;
      }
      confirmedSize = undefined;
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

  async function settle() {
    await globalThis.__bperf?.settle?.();
  }

  function doctorProbe() {
    Reflect.set(
      globalThis,
      "__bperfDoctorHeap",
      Array.from({ length: 20_000 }, (_, index) => ({
        index,
        marker: `bperf-doctor-${index}`,
        payload: new Array(20).fill(index),
      })),
    );

    let total = 0;
    const deadline = performance.now() + 750;
    function bperfDoctorHotLoop() {
      for (let index = 0; index < 50_000; index += 1) {
        total += Math.sqrt(index % 1_000);
      }
    }
    while (performance.now() < deadline) {
      bperfDoctorHotLoop();
    }
    return total;
  }

  function runtimeAnchor() {
    const values = new Uint32Array(4_096);
    for (let index = 0; index < values.length; index += 1) {
      values[index] = Math.imul(index + 1, 2_654_435_761) >>> 0;
    }

    function run(rounds) {
      let checksum = 2_166_136_261;
      for (let round = 0; round < rounds; round += 1) {
        for (let index = 0; index < values.length; index += 1) {
          const value = values[(index + round) & (values.length - 1)];
          checksum = Math.imul(checksum ^ value, 16_777_619) >>> 0;
        }
      }
      return checksum;
    }

    run(RUNTIME_ANCHOR.baseRounds);
    const calibrationStarted = performance.now();
    run(RUNTIME_ANCHOR.baseRounds);
    const calibrationMs = performance.now() - calibrationStarted;
    const batchSize = Math.max(
      1,
      Math.min(
        RUNTIME_ANCHOR.maxBatchSize,
        Math.ceil(RUNTIME_ANCHOR.targetSampleMs / calibrationMs),
      ),
    );
    const rounds = RUNTIME_ANCHOR.baseRounds * batchSize;

    for (
      let index = 0;
      index < RUNTIME_ANCHOR.warmupSamples;
      index += 1
    ) {
      run(rounds);
    }

    const wallMs = [];
    let checksum = 0;
    for (
      let index = 0;
      index < RUNTIME_ANCHOR.samples;
      index += 1
    ) {
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
      workload: RUNTIME_ANCHOR.workload,
      wall_ms: wallMs,
      batch_size: batchSize,
      checksum,
    };
  }

  const existing = globalThis.__bperfHarness;
  if (existing !== undefined && existing.version !== VERSION) {
    throw new Error(
      `bperf browser workload mismatch: expected ${VERSION}, received ${
        String(existing.version)
      }`,
    );
  }
  globalThis.__bperfHarness ??= Object.freeze({
    version: VERSION,
    prepare,
    execute,
    selectBatchSize,
    settle,
    doctorProbe,
    runtimeAnchor,
  });
})();
