import fs from "node:fs";
import path from "node:path";
import process from "node:process";
import { spawnSync } from "node:child_process";

const repository = path.resolve(import.meta.dirname, "..");
const kind = process.argv[2];
if (kind !== "release" && kind !== "source") {
  throw new Error("usage: verify-install.ts <release|source>");
}
const cargoHome = requiredDirectory("BPERF_TEST_CARGO_HOME");
const scratch = requiredDirectory("BPERF_TEST_ROOT");
const executable = path.join(
  cargoHome,
  "bin",
  process.platform === "win32" ? "bperf.exe" : "bperf",
);
const manifest = fs.readFileSync(
  path.join(repository, "Cargo.toml"),
  "utf8",
);
const version = manifest.match(/^version\s*=\s*"([^"]+)"/m)?.[1];
if (!version) {
  throw new Error("Cargo.toml has no package version");
}
const runtime = path.join(
  cargoHome,
  "bin",
  "bperf-runtime",
  version,
  "sidecar",
);
if (fs.existsSync(runtime)) {
  throw new Error(
    `${kind} install smoke must begin without an external runtime: ${runtime}`,
  );
}

run(["--version"]);
if (kind === "release") {
  run([
    "browsers",
    "install",
    "--engine",
    "all",
    ...(process.platform === "linux" ? ["--with-deps"] : []),
  ]);
} else {
  run(["browsers", "install", "--engine", "chromium"]);
}
run([
  "doctor",
  "--engine",
  kind === "release" ? "all" : "chromium",
  "--artifact-dir",
  path.join(scratch, "doctor"),
]);

for (const relative of [
  "src/benchmark-host.ts",
  "src/browser-benchmark.ts",
  "src/project-modules.ts",
  "node_modules/esbuild/package.json",
  "node_modules/playwright-core/browsers.json",
]) {
  const required = path.join(runtime, relative);
  if (!fs.statSync(required, { throwIfNoEntry: false })?.isFile()) {
    throw new Error(`installed package did not materialize ${required}`);
  }
}

if (kind === "release") {
  run([
    "run",
    path.join(repository, "examples", "managed", "fragment-parser.bench.ts"),
    "--budget",
    "30s",
    "--message",
    "Verify installed release package",
    "--artifact-dir",
    path.join(scratch, "measurements"),
    "--state-dir",
    path.join(scratch, "managed"),
    "--object-dir",
    path.join(scratch, "objects"),
    "--registry-dir",
    path.join(scratch, "baselines"),
    "--comparison-dir",
    path.join(scratch, "comparisons"),
    "--lineage-dir",
    path.join(scratch, "lineages"),
  ]);
}

function run(arguments_: string[]): void {
  const environment = { ...process.env };
  delete environment.BPERF_SIDECAR_DIR;
  const result = spawnSync(executable, arguments_, {
    cwd: repository,
    env: environment,
    stdio: "inherit",
  });
  if (result.error) {
    throw result.error;
  }
  if (result.status !== 0) {
    throw new Error(
      `${executable} ${arguments_.join(" ")} exited with status ${String(result.status)}`,
    );
  }
}

function requiredDirectory(name: string): string {
  const value = process.env[name];
  if (!value) {
    throw new Error(`${name} is required`);
  }
  const resolved = path.resolve(value);
  fs.mkdirSync(resolved, { recursive: true });
  return resolved;
}
