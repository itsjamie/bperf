import {
  defineBrowserBenchmark,
  exact,
  readBufferDigest,
} from "bperf/browser";

import { createIntegerHasher } from "./integer-hash.ts";

export default defineBrowserBenchmark({
  id: "webgpu-integer-hash",
  engines: ["chromium"],
  gpu: "webgpu",

  cases: [
    {
      id: "values-262144",

      async setup() {
        const adapter = await navigator.gpu.requestAdapter() ??
          await navigator.gpu.requestAdapter({ forceFallbackAdapter: true });
        if (!adapter) throw new Error("WebGPU is unavailable");
        return createIntegerHasher(await adapter.requestDevice(), 262_144);
      },

      // Mapping the output back waits for the dispatch, and the digest proves
      // every value was hashed.
      async measure(hasher) {
        hasher.hash();
        return await readBufferDigest(hasher.device, hasher.output);
      },

      expect: exact("dbc581d288e99e5c"),
    },
  ],
});
