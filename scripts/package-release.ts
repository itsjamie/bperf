import { createHash } from "node:crypto";
import {
  chmod,
  copyFile,
  cp,
  mkdir,
  readFile,
  rename,
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

const platformNames: Record<string, string> = {
  darwin: "macos",
  linux: "linux",
  win32: "windows",
};
const platformName = platformNames[process.platform] ?? process.platform;
const executableName = process.platform === "win32" ? "bperf.exe" : "bperf";
const bundleName = `bperf-${version}-${platformName}-${process.arch}`;
const distributionRoot = path.join(repository, "dist");
const bundle = path.join(distributionRoot, bundleName);
assertChild(distributionRoot, bundle);

run(process.platform === "win32" ? "cargo.exe" : "cargo", [
  "build",
  "--release",
  "--locked",
]);

await rm(bundle, { recursive: true, force: true });
await mkdir(path.join(bundle, "sidecar"), { recursive: true });
await copyFile(
  path.join(repository, "target", "release", executableName),
  path.join(bundle, executableName),
);
await cp(
  path.join(repository, "sidecar", "src"),
  path.join(bundle, "sidecar", "src"),
  { recursive: true },
);
for (const name of ["package.json", "package-lock.json"]) {
  await copyFile(
    path.join(repository, "sidecar", name),
    path.join(bundle, "sidecar", name),
  );
}
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
  path.join(bundle, "sidecar"),
  { PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD: "1" },
);

const executable = path.join(bundle, executableName);
const executableSha256 = createHash("sha256")
  .update(await readFile(executable))
  .digest("hex");
await writeFile(
  path.join(bundle, "BUILD.json"),
  `${JSON.stringify(
    {
      schema_version: 1,
      name: "bperf",
      version,
      platform: process.platform,
      architecture: process.arch,
      node: ">=24.12.0",
      executable_sha256: executableSha256,
    },
    null,
    2,
  )}\n`,
);

if (process.argv.includes("--install")) {
  await install(bundle, version, executableName);
}

console.log(bundle);

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
    process.env.CARGO_HOME ?? path.join(homedir(), ".cargo"),
  );
  const binaryDirectory = path.join(cargoHome, "bin");
  const runtimeRoot = path.join(
    binaryDirectory,
    "bperf-runtime",
    releaseVersion,
  );
  const stagedRuntime = `${runtimeRoot}.staging-${process.pid}`;
  assertChild(binaryDirectory, runtimeRoot);
  assertChild(binaryDirectory, stagedRuntime);

  await mkdir(binaryDirectory, { recursive: true });
  await rm(stagedRuntime, { recursive: true, force: true });
  await cp(path.join(source, "sidecar"), path.join(stagedRuntime, "sidecar"), {
    recursive: true,
  });
  await rm(runtimeRoot, { recursive: true, force: true });
  await rename(stagedRuntime, runtimeRoot);
  const installedExecutable = path.join(binaryDirectory, releaseExecutable);
  await copyFile(path.join(source, releaseExecutable), installedExecutable);
  if (process.platform !== "win32") {
    await chmod(installedExecutable, 0o755);
  }
}

function assertChild(parent: string, child: string): void {
  const relative = path.relative(path.resolve(parent), path.resolve(child));
  if (relative === "" || relative.startsWith("..") || path.isAbsolute(relative)) {
    throw new Error(`refusing path outside ${parent}: ${child}`);
  }
}
