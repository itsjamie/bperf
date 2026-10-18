// This source executes in every benchmark realm and cannot depend on host APIs.
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

  // Each sample spans enough draws that the coarse page clock and GPU clock
  // ramps stay well below the comparator's drift threshold.
  const GPU_ANCHOR = Object.freeze({
    samples: 31,
    warmupSamples: 8,
    maxBatchSize: 1_000_000,
    targetSampleMs: 75,
    size: 256,
    workloads: Object.freeze({
      webgl: "webgl_fill_v1",
      webgl2: "webgl2_fill_v1",
      webgpu: "webgpu_compute_v1",
    }),
  });
  // Renderer and architecture strings of CPU rasterizers. A fallback WebGPU
  // adapter is software regardless of its name.
  const SOFTWARE_GPU = /swiftshader|llvmpipe|lavapipe|softpipe|microsoft basic render|\bwarp\b/i;

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

  function webglContext(api, width, height) {
    const gl = new OffscreenCanvas(width, height).getContext(api, {
      antialias: false,
      preserveDrawingBuffer: false,
    });
    if (!gl) throw new Error(`${api} context creation failed`);
    return gl;
  }

  // Without a usable hardware adapter, the browser's fallback (software)
  // adapter still satisfies the requirement; gpuProbe reports it as software.
  async function webgpuDevice() {
    const gpu = globalThis.navigator?.gpu;
    if (!gpu) throw new Error("webgpu is unavailable: navigator.gpu is missing");
    const adapter = await gpu.requestAdapter() ??
      await gpu.requestAdapter({ forceFallbackAdapter: true });
    if (!adapter) throw new Error("webgpu is unavailable: no adapter");
    return { adapter, device: await adapter.requestDevice() };
  }

  async function gpuProbe(api) {
    if (api === "webgpu") {
      const { adapter, device } = await webgpuDevice();
      device.destroy();
      const info = adapter.info ?? {};
      const renderer = [info.vendor, info.architecture, info.device, info.description]
        .filter((part) => typeof part === "string" && part.length > 0)
        .join(" ") || "unknown";
      return {
        api,
        vendor: info.vendor || "unknown",
        renderer,
        software: Boolean(info.isFallbackAdapter) || SOFTWARE_GPU.test(renderer),
      };
    }
    const gl = webglContext(api, 1, 1);
    const debug = gl.getExtension("WEBGL_debug_renderer_info");
    const vendor = String(gl.getParameter(debug ? debug.UNMASKED_VENDOR_WEBGL : gl.VENDOR));
    const renderer = String(
      gl.getParameter(debug ? debug.UNMASKED_RENDERER_WEBGL : gl.RENDERER),
    );
    gl.getExtension("WEBGL_lose_context")?.loseContext();
    return { api, vendor, renderer, software: SOFTWARE_GPU.test(renderer) };
  }

  // Draws a fixed fragment workload; the single-pixel readback waits for the
  // GPU, so each call times submitted and completed work.
  function webglAnchorRunner(api) {
    const size = GPU_ANCHOR.size;
    const gl = webglContext(api, size, size);
    function shader(type, source) {
      const compiled = gl.createShader(type);
      gl.shaderSource(compiled, source);
      gl.compileShader(compiled);
      if (!gl.getShaderParameter(compiled, gl.COMPILE_STATUS)) {
        throw new Error(`GPU anchor shader failed: ${gl.getShaderInfoLog(compiled)}`);
      }
      return compiled;
    }
    const program = gl.createProgram();
    gl.attachShader(program, shader(gl.VERTEX_SHADER,
      "attribute vec2 p; void main() { gl_Position = vec4(p, 0.0, 1.0); }"));
    gl.attachShader(program, shader(gl.FRAGMENT_SHADER, `precision highp float;
uniform float seed;
void main() {
  vec2 c = gl_FragCoord.xy / ${size}.0;
  float v = seed;
  for (int i = 0; i < 64; i++) {
    v = fract(sin(v * 12.9898 + c.x * 78.233 + c.y * 37.719) * 43758.5453);
  }
  gl_FragColor = vec4(v, v * 0.5, 1.0 - v, 1.0);
}`));
    gl.linkProgram(program);
    if (!gl.getProgramParameter(program, gl.LINK_STATUS)) {
      throw new Error(`GPU anchor program failed: ${gl.getProgramInfoLog(program)}`);
    }
    gl.useProgram(program);
    gl.bindBuffer(gl.ARRAY_BUFFER, gl.createBuffer());
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 3, -1, -1, 3]), gl.STATIC_DRAW);
    const position = gl.getAttribLocation(program, "p");
    gl.enableVertexAttribArray(position);
    gl.vertexAttribPointer(position, 2, gl.FLOAT, false, 0, 0);
    const seed = gl.getUniformLocation(program, "seed");
    gl.viewport(0, 0, size, size);
    const pixel = new Uint8Array(4);
    return async (draws) => {
      for (let draw = 0; draw < draws; draw += 1) {
        gl.uniform1f(seed, draw % 7);
        gl.drawArrays(gl.TRIANGLES, 0, 3);
      }
      gl.readPixels(size / 2, size / 2, 1, 1, gl.RGBA, gl.UNSIGNED_BYTE, pixel);
      if (gl.isContextLost()) throw new Error("GPU anchor context was lost");
      return ((pixel[0] << 24) | (pixel[1] << 16) | (pixel[2] << 8) | pixel[3]) >>> 0;
    };
  }

  // Integer hashing in a compute shader is exact on every adapter; mapping
  // the first word back waits for the dispatches to finish.
  async function webgpuAnchorRunner() {
    const { device } = await webgpuDevice();
    const count = 65_536;
    const storage = device.createBuffer({
      size: count * 4,
      usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_SRC | GPUBufferUsage.COPY_DST,
    });
    const readback = device.createBuffer({
      size: 4,
      usage: GPUBufferUsage.MAP_READ | GPUBufferUsage.COPY_DST,
    });
    const module = device.createShaderModule({
      code: `@group(0) @binding(0) var<storage, read_write> data: array<u32>;
@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
  var v = data[id.x] ^ id.x;
  for (var i = 0u; i < 64u; i = i + 1u) {
    v = v * 1664525u + 1013904223u;
    v = v ^ (v >> 16u);
  }
  data[id.x] = v;
}`,
    });
    const pipeline = device.createComputePipeline({
      layout: "auto",
      compute: { module, entryPoint: "main" },
    });
    const bindings = device.createBindGroup({
      layout: pipeline.getBindGroupLayout(0),
      entries: [{ binding: 0, resource: { buffer: storage } }],
    });
    const zeros = new Uint32Array(count);
    return async (draws) => {
      device.queue.writeBuffer(storage, 0, zeros);
      const encoder = device.createCommandEncoder();
      const pass = encoder.beginComputePass();
      pass.setPipeline(pipeline);
      pass.setBindGroup(0, bindings);
      for (let draw = 0; draw < draws; draw += 1) {
        pass.dispatchWorkgroups(count / 64);
      }
      pass.end();
      encoder.copyBufferToBuffer(storage, 0, readback, 0, 4);
      device.queue.submit([encoder.finish()]);
      await readback.mapAsync(GPUMapMode.READ);
      const value = new Uint32Array(readback.getMappedRange())[0];
      readback.unmap();
      return value;
    };
  }

  async function gpuAnchor(api) {
    const workload = GPU_ANCHOR.workloads[api];
    if (workload === undefined) throw new Error(`unknown GPU API ${String(api)}`);
    const run = api === "webgpu" ? await webgpuAnchorRunner() : webglAnchorRunner(api);
    await run(1);
    // A single draw is dominated by the readback's fixed cost, so grow the
    // batch until one sample actually reaches the target duration.
    let batchSize = 1;
    while (batchSize < GPU_ANCHOR.maxBatchSize) {
      const started = performance.now();
      await run(batchSize);
      const elapsed = performance.now() - started;
      if (elapsed >= GPU_ANCHOR.targetSampleMs) break;
      batchSize = Math.min(
        GPU_ANCHOR.maxBatchSize,
        Math.max(
          batchSize * 2,
          Math.ceil(batchSize * GPU_ANCHOR.targetSampleMs / Math.max(elapsed, 0.1)),
        ),
      );
    }
    for (let index = 0; index < GPU_ANCHOR.warmupSamples; index += 1) {
      await run(batchSize);
    }
    const wallMs = [];
    let checksum = 0;
    for (let index = 0; index < GPU_ANCHOR.samples; index += 1) {
      const started = performance.now();
      const result = await run(batchSize);
      wallMs.push((performance.now() - started) / batchSize);
      if (index > 0 && result !== checksum) {
        throw new Error("GPU anchor produced an unstable checksum");
      }
      checksum = result;
    }
    return { workload, wall_ms: wallMs, batch_size: batchSize, checksum };
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
    gpuProbe,
    gpuAnchor,
  });
})();
