import {
  defineBrowserBenchmark,
  exact,
  readPixelsDigest,
} from "bperf/browser";

import { createTileRenderer } from "./tile-renderer.ts";

export default defineBrowserBenchmark({
  id: "webgl-tile-renderer",
  engines: ["chromium"],
  gpu: "webgl",

  cases: [
    {
      id: "grid-64",

      setup() {
        const canvas = new OffscreenCanvas(256, 256);
        const gl = canvas.getContext("webgl", { antialias: false });
        if (!gl) throw new Error("WebGL is unavailable");
        return createTileRenderer(gl, 64);
      },

      // The readback waits for the GPU, so wall time covers the finished
      // frame, and the digest proves every tile was drawn.
      measure(renderer) {
        renderer.draw();
        return readPixelsDigest(renderer.gl);
      },

      expect: exact("b912e7e5f3bbc625"),
    },
  ],
});
