// Hashes a buffer of 32-bit integers on the GPU. Integer arithmetic gives the
// same bytes on every WebGPU adapter, so the result can be checked exactly.

const SHADER = `
@group(0) @binding(0) var<storage, read_write> values: array<u32>;

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
  if (id.x >= arrayLength(&values)) {
    return;
  }
  var value = values[id.x] ^ id.x;
  for (var round = 0u; round < 32u; round = round + 1u) {
    value = value * 1664525u + 1013904223u;
    value = value ^ (value >> 13u);
  }
  values[id.x] = value;
}`;

export interface IntegerHasher {
  readonly device: GPUDevice;
  readonly output: GPUBuffer;
  hash(): void;
}

export function createIntegerHasher(device: GPUDevice, count: number): IntegerHasher {
  const output = device.createBuffer({
    size: count * 4,
    usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_SRC | GPUBufferUsage.COPY_DST,
  });
  const input = new Uint32Array(count).map((_, index) => index * 2_654_435_761);
  const pipeline = device.createComputePipeline({
    layout: "auto",
    compute: { module: device.createShaderModule({ code: SHADER }), entryPoint: "main" },
  });
  const bindings = device.createBindGroup({
    layout: pipeline.getBindGroupLayout(0),
    entries: [{ binding: 0, resource: { buffer: output } }],
  });
  return {
    device,
    output,
    hash() {
      device.queue.writeBuffer(output, 0, input);
      const encoder = device.createCommandEncoder();
      const pass = encoder.beginComputePass();
      pass.setPipeline(pipeline);
      pass.setBindGroup(0, bindings);
      pass.dispatchWorkgroups(Math.ceil(count / 64));
      pass.end();
      device.queue.submit([encoder.finish()]);
    },
  };
}
