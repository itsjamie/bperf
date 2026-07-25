import type { Page } from "playwright";

export function runProbeWorkload(page: Page): Promise<number> {
  return page.evaluate(() => {
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
  });
}
