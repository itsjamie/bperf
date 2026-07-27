import { createHash } from "node:crypto";
import {
  chmod,
  copyFile,
  cp,
  mkdir,
  readFile,
  rm,
  writeFile,
} from "node:fs/promises";
import { homedir } from "node:os";
import path from "node:path";
import { spawnSync } from "node:child_process";
import process from "node:process";

const repository = path.resolve(import.meta.dirname, "..");
const cargoManifest = await readFile(
  path.join(repository, "Cargo.toml"),
  "utf8",
);
const version = cargoManifest.match(/^version\s*=\s*"([^"]+)"/m)?.[1];
if (!version) {
  throw new Error("Cargo.toml has no package version");
}
if (
  process.env.GITHUB_REF_TYPE === "tag" &&
  process.env.GITHUB_REF_NAME !== `v${version}`
) {
  throw new Error(
    `release tag ${String(process.env.GITHUB_REF_NAME)} does not match Cargo version v${version}`,
  );
}

const hostTarget = rustHostTarget();
const target = process.env.BPERF_RELEASE_TARGET ?? hostTarget;
if (target !== hostTarget) {
  throw new Error(
    `release target ${target} does not match this native runner (${hostTarget})`,
  );
}
const executableName = process.platform === "win32" ? "bperf.exe" : "bperf";
const bundleName = `bperf-${version}-${target}`;
const distributionRoot = path.join(repository, "dist");
const bundle = path.join(distributionRoot, bundleName);
const archiveName = `${bundleName}.tar.gz`;
const archive = path.join(distributionRoot, archiveName);
const runtimeStage = path.join(distributionRoot, `.runtime-${target}`);
const runtimeSidecar = path.join(runtimeStage, "sidecar");
assertChild(distributionRoot, bundle);
assertChild(distributionRoot, archive);
assertChild(distributionRoot, runtimeStage);

await rm(runtimeStage, { recursive: true, force: true });
await mkdir(path.join(runtimeSidecar, "src"), { recursive: true });
for (const name of [
  "benchmark-host.ts",
  "browser-benchmark.ts",
  "project-modules.ts",
]) {
  await copyFile(
    path.join(repository, "sidecar", "src", name),
    path.join(runtimeSidecar, "src", name),
  );
}
for (const name of ["package.json", "package-lock.json"]) {
  await copyFile(
    path.join(repository, "sidecar", name),
    path.join(runtimeSidecar, name),
  );
}
installProductionDependencies(runtimeSidecar);

run(process.platform === "win32" ? "cargo.exe" : "cargo", [
  "build",
  "--release",
  "--locked",
  "--target",
  target,
], repository, {
  BPERF_EMBEDDED_SIDECAR_DIR: runtimeSidecar,
});

await rm(bundle, { recursive: true, force: true });
await rm(archive, { force: true });
await mkdir(bundle, { recursive: true });
await copyFile(
  path.join(repository, "target", target, "release", executableName),
  path.join(bundle, executableName),
);
for (const name of [
  "README.md",
  "CONTRIBUTING.md",
  "LICENSE",
]) {
  await copyFile(
    path.join(repository, name),
    path.join(bundle, name),
  );
}
await cp(
  path.join(repository, "docs"),
  path.join(bundle, "docs"),
  { recursive: true },
);
await cp(
  path.join(repository, "examples"),
  path.join(bundle, "examples"),
  { recursive: true },
);
await cp(
  path.join(repository, "skills", "bperf-agent-loop"),
  path.join(bundle, "skills", "bperf-agent-loop"),
  { recursive: true },
);

const executable = path.join(bundle, executableName);
const executableSha256 = createHash("sha256")
  .update(await readFile(executable))
  .digest("hex");
await writeFile(
  path.join(bundle, "BUILD.json"),
  `${JSON.stringify(
    {
      schema_version: 2,
      name: "bperf",
      version,
      platform: process.platform,
      architecture: process.arch,
      target,
      node: ">=24.12.0",
      embedded_benchmark_runtime: true,
      browser_adapters: {
        chromium: "rust-chromium",
        firefox: "rust-firefox",
        webkit: "rust-webkit",
      },
      protocols: {
        capture: 13,
        benchmark_host: 2,
        environment_schema: 6,
        doctor_schema: 2,
      },
      executable_sha256: executableSha256,
    },
    null,
    2,
  )}\n`,
);

if (process.argv.includes("--install")) {
  await install(bundle, version, executableName);
}

run("tar", ["-czf", archiveName, bundleName], distributionRoot);
const archiveSha256 = createHash("sha256")
  .update(await readFile(archive))
  .digest("hex");
await writeFile(
  `${archive}.sha256`,
  `${archiveSha256}  ${archiveName}\n`,
);
await rm(runtimeStage, { recursive: true, force: true });

console.log(archive);

function run(
  command: string,
  arguments_: string[],
  cwd = repository,
  environment: Record<string, string> = {},
): void {
  const result = spawnSync(command, arguments_, {
    cwd,
    env: { ...process.env, ...environment },
    stdio: "inherit",
  });
  if (result.error) {
    throw result.error;
  }
  if (result.status !== 0) {
    throw new Error(`${command} exited with status ${String(result.status)}`);
  }
}

async function install(
  source: string,
  releaseVersion: string,
  releaseExecutable: string,
): Promise<void> {
  const cargoHome = path.resolve(
    process.env.BPERF_INSTALL_ROOT ??
      process.env.CARGO_HOME ??
      path.join(homedir(), ".cargo"),
  );
  const binaryDirectory = path.join(cargoHome, "bin");
  const runtimeRoot = path.join(
    binaryDirectory,
    "bperf-runtime",
    releaseVersion,
  );
  assertChild(binaryDirectory, runtimeRoot);

  await mkdir(binaryDirectory, { recursive: true });
  await rm(runtimeRoot, { recursive: true, force: true });
  const installedExecutable = path.join(binaryDirectory, releaseExecutable);
  await copyFile(path.join(source, releaseExecutable), installedExecutable);
  if (process.platform !== "win32") {
    await chmod(installedExecutable, 0o755);
  }
}

function installProductionDependencies(sidecar: string): void {
  const npmCommand =
    process.platform === "win32"
      ? {
          executable: process.execPath,
          arguments: [
            path.join(
              path.dirname(process.execPath),
              "node_modules",
              "npm",
              "bin",
              "npm-cli.js",
            ),
          ],
        }
      : { executable: "npm", arguments: [] };
  run(
    npmCommand.executable,
    [
      ...npmCommand.arguments,
      "ci",
      "--omit=dev",
      "--no-audit",
      "--no-fund",
    ],
    sidecar,
    { PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD: "1" },
  );
}

function rustHostTarget(): string {
  const result = spawnSync("rustc", ["-vV"], {
    cwd: repository,
    encoding: "utf8",
  });
  if (result.error) {
    throw result.error;
  }
  if (result.status !== 0) {
    throw new Error(`rustc -vV exited with status ${String(result.status)}`);
  }
  const target = result.stdout
    .split(/\r?\n/)
    .find((line) => line.startsWith("host: "))
    ?.slice("host: ".length);
  if (!target) {
    throw new Error("rustc -vV did not report a host target");
  }
  return target;
}

function assertChild(parent: string, child: string): void {
  const relative = path.relative(path.resolve(parent), path.resolve(child));
  if (relative === "" || relative.startsWith("..") || path.isAbsolute(relative)) {
    throw new Error(`refusing path outside ${parent}: ${child}`);
  }
}
