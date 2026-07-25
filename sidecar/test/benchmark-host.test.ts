import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import test from "node:test";

import { startBenchmarkHost } from "../src/benchmark-host.ts";

async function withTemporaryProject(
  prefix: string,
  action: (root: string) => Promise<void>,
): Promise<void> {
  const root = fs.mkdtempSync(path.join(process.cwd(), prefix));
  try {
    await action(root);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
}

test("managed host serves transformed modules and ranged fixtures", async () => {
  await withTemporaryProject(".bperf-host-", async (root) => {
    const benchmark = path.join(root, "sample.bench.ts");
    const fixtureBody = Buffer.from("0123456789");
    const bodyPath = path.join(root, "fixture-body");
    const lockPath = path.join(root, "fixture-lock.json");
    const descriptor = {
      source: "./segment.txt",
      response: { contentType: "text/plain" },
    };
    fs.writeFileSync(
      benchmark,
      "const value: number = 42;\nexport default value;\n",
    );
    fs.writeFileSync(bodyPath, fixtureBody);
    fs.writeFileSync(
      lockPath,
      JSON.stringify({
        schema_version: 1,
        fixtures: [
          {
            descriptor,
            body_path: bodyPath,
            sha256: "unused-by-host",
            size_bytes: fixtureBody.length,
            content_type: "text/plain",
          },
        ],
      }),
    );

    const host = await startBenchmarkHost({
      root,
      benchmark,
      fixtureLock: lockPath,
    });
    try {
      const moduleResponse = await fetch(
        `${host.origin}/__bperf/benchmark.js`,
      );
      assert.equal(moduleResponse.status, 200);
      const moduleSource = await moduleResponse.text();
      assert.match(moduleSource, /var value\s*=\s*42/);
      assert.doesNotMatch(moduleSource, /: number/);

      const url = new URL("/__bperf/fixture", host.origin);
      url.searchParams.set("descriptor", JSON.stringify(descriptor));
      const fixtureResponse = await fetch(url, {
        headers: { range: "bytes=2-5" },
      });
      assert.equal(fixtureResponse.status, 206);
      assert.equal(fixtureResponse.headers.get("accept-ranges"), "bytes");
      assert.equal(
        fixtureResponse.headers.get("content-range"),
        "bytes 2-5/10",
      );
      assert.equal(await fixtureResponse.text(), "2345");
    } finally {
      await host.close();
    }
  });
});

test("managed host resolves installed ESM packages from each importer", async () => {
  await withTemporaryProject(".bperf-packages-", async (root) => {
    const benchmark = path.join(root, "sample.bench.ts");
    const packageRoot = path.join(
      root,
      "node_modules",
      "example-package",
    );
    const dependencyRoot = path.join(
      root,
      "node_modules",
      "example-dependency",
    );
    fs.mkdirSync(packageRoot, { recursive: true });
    fs.mkdirSync(dependencyRoot, { recursive: true });
    fs.writeFileSync(
      benchmark,
      [
        'import { exact } from "bperf/browser";',
        'import { value } from "example-package";',
        'export const lazy = () => import("example-package");',
        "export default exact(value);",
      ].join("\n"),
    );
    fs.writeFileSync(
      path.join(packageRoot, "package.json"),
      JSON.stringify({
        name: "example-package",
        type: "module",
        exports: "./index.js",
      }),
    );
    fs.writeFileSync(
      path.join(packageRoot, "index.js"),
      [
        'import { nested } from "example-dependency";',
        "export const value = nested;",
      ].join("\n"),
    );
    fs.writeFileSync(
      path.join(dependencyRoot, "package.json"),
      JSON.stringify({
        name: "example-dependency",
        type: "module",
        exports: "./index.js",
      }),
    );
    fs.writeFileSync(
      path.join(dependencyRoot, "index.js"),
      "export const nested = 42;\n",
    );

    const host = await startBenchmarkHost({ root, benchmark });
    try {
      const benchmarkResponse = await fetch(
        `${host.origin}/__bperf/benchmark.js`,
      );
      assert.equal(benchmarkResponse.status, 200);
      const benchmarkSource = await benchmarkResponse.text();
      assert.match(
        benchmarkSource,
        /node_modules\/example-package\/index\.js/,
      );
      assert.match(
        benchmarkSource,
        /node_modules\/example-dependency\/index\.js/,
      );
      assert.match(benchmarkSource, /nested = 42/);
      assert.match(benchmarkSource, /from "bperf\/browser"/);
      assert.doesNotMatch(
        benchmarkSource,
        /from "example-(?:package|dependency)"/,
      );
      assert.equal(
        host.sourceFiles.has(
          fs.realpathSync(path.join(packageRoot, "index.js")),
        ),
        true,
      );
      assert.equal(
        host.sourceFiles.has(
          fs.realpathSync(path.join(dependencyRoot, "index.js")),
        ),
        true,
      );
    } finally {
      await host.close();
    }
  });
});
