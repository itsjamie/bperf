import assert from "node:assert/strict";
import test from "node:test";

import {
  positiveWeights,
  SpeedscopeBuilder,
} from "../src/speedscope.ts";

test("positiveWeights replaces terminal and non-positive deltas", () => {
  assert.deepEqual(positiveWeights([1, 3, 3, 8], 1), [2, 5, 5, 5]);
});

test("SpeedscopeBuilder emits a sampled-profile document", () => {
  const builder = new SpeedscopeBuilder("test");
  const root = builder.frame({ name: "root" });
  const leaf = builder.frame({
    name: "leaf",
    file: "fixture.js",
    line: 1,
  });
  builder.sampledProfile({
    name: "profile",
    unit: "milliseconds",
    samples: [
      [root, leaf],
      [root],
    ],
    weights: [1, 2],
  });

  const document = builder.document();
  assert.equal(
    document.$schema,
    "https://www.speedscope.app/file-format-schema.json",
  );
  assert.equal(document.shared.frames.length, 2);
  assert.equal(document.profiles[0]?.endValue, 3);
});

test("SpeedscopeBuilder rejects empty samples", () => {
  const builder = new SpeedscopeBuilder("test");
  assert.throws(
    () =>
      builder.sampledProfile({
        name: "empty",
        unit: "seconds",
        samples: [],
        weights: [],
      }),
    /Invalid Speedscope sample data/,
  );
});
