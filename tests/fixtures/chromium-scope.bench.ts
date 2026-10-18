import { defineBrowserBenchmark, exact } from "bperf/browser";

export default defineBrowserBenchmark({
  id: "chromium-scope",
  engines: ["chromium"],

  cases: [
    {
      id: "triangular-sum",

      measure() {
        let total = 0;
        for (let value = 0; value < 200_000; value += 1) {
          total += value;
        }
        return total;
      },

      expect: exact(19_999_900_000),
    },
  ],
});
