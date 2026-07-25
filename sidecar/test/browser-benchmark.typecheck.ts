import {
  defineBrowserBenchmark,
  exact,
} from "../src/browser-benchmark.ts";

type IsAny<Value> = 0 extends 1 & Value ? true : false;
type AssertFalse<Condition extends false> = Condition;

defineBrowserBenchmark({
  id: "setup-state-inference",
  cases: [
    {
      id: "stateful-case",
      setup() {
        return { value: 41 };
      },
      measure(state) {
        type StateIsNotAny = AssertFalse<IsAny<typeof state>>;
        const notAny: StateIsNotAny = false;
        const inferred: { value: number } = state;
        void notAny;
        void inferred;
        return { value: state.value + 1 };
      },
      expect: exact({ value: 42 }),
    },
    {
      id: "heterogeneous-state",
      setup() {
        return "browser";
      },
      measure(state) {
        type StateIsNotAny = AssertFalse<IsAny<typeof state>>;
        const notAny: StateIsNotAny = false;
        const inferred: string = state;
        void notAny;
        void inferred;
        return state.toUpperCase();
      },
      expect: exact("BROWSER"),
    },
  ],
});
