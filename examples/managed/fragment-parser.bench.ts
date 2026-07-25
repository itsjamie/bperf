import {
  defineBrowserBenchmark,
  exact,
  fixture,
} from "bperf/browser";

import { parseFragmentStream } from "./fragment-parser.ts";

const fragment = fixture("./fixtures/segment.txt", {
  response: {
    contentType: "application/octet-stream",
    stream: {
      chunkSize: 64,
    },
  },
});

export default defineBrowserBenchmark({
  id: "fragment-parser",

  cases: [
    {
      id: "representative-fragment",

      async measure() {
        const response = await fetch(fragment.url);
        if (!response.body) {
          throw new Error("fragment response has no body");
        }
        return parseFragmentStream(response.body);
      },

      expect: exact({
        byteLength: 556,
        checksum: 663721477,
      }),
    },
  ],
});
