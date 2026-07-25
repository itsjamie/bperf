import fs from "node:fs";
import path from "node:path";
import { stripTypeScriptTypes } from "node:module";
import { pathToFileURL } from "node:url";

import { build } from "esbuild";

export interface BrowserProject {
  readonly root: string;
  readonly sourceFiles: ReadonlySet<string>;
  resolveFile(target: string, label: string): string;
  browserEntry(filePath: string): Promise<string>;
  browserModule(filePath: string): string;
}

export interface BrowserProjectOptions {
  virtualImports?: readonly string[];
}

const resolutionFiles = [
  "package.json",
  "package-lock.json",
  "npm-shrinkwrap.json",
  "pnpm-lock.yaml",
  "yarn.lock",
  "bun.lock",
  "bun.lockb",
  "tsconfig.json",
  "jsconfig.json",
];

function canonical(filePath: string): string {
  return fs.realpathSync(filePath);
}

function isWithin(root: string, target: string): boolean {
  const relative = path.relative(root, target);
  return (
    relative === "" ||
    (!relative.startsWith(`..${path.sep}`) &&
      relative !== ".." &&
      !path.isAbsolute(relative))
  );
}

function javascript(filePath: string): string {
  const source = fs.readFileSync(filePath, "utf8");
  if (![".ts", ".tsx", ".mts"].includes(path.extname(filePath))) {
    return source;
  }
  return stripTypeScriptTypes(source, {
    mode: "transform",
    sourceMap: false,
    sourceUrl: pathToFileURL(filePath).href,
  });
}

class ProjectModules implements BrowserProject {
  readonly root: string;
  readonly sourceFiles = new Set<string>();
  readonly virtualImports: readonly string[];

  constructor(root: string, options: BrowserProjectOptions) {
    this.root = canonical(root);
    this.virtualImports = options.virtualImports ?? [];
  }

  resolveFile(target: string, label: string): string {
    const resolved = canonical(target);
    if (!isWithin(this.root, resolved)) {
      throw new Error(`${label} is outside benchmark root: ${target}`);
    }
    return resolved;
  }

  async browserEntry(filePath: string): Promise<string> {
    const entry = this.resolveFile(filePath, "benchmark module");
    const entryPoint =
      `./${path.relative(this.root, entry).split(path.sep).join("/")}`;
    const result = await build({
      absWorkingDir: this.root,
      bundle: true,
      entryPoints: [entryPoint],
      external: [...this.virtualImports],
      format: "esm",
      legalComments: "none",
      logLevel: "silent",
      metafile: true,
      platform: "browser",
      sourcemap: "inline",
      target: "es2022",
      treeShaking: true,
      write: false,
    });
    const outputs = result.outputFiles ?? [];
    if (outputs.length !== 1 || !result.metafile) {
      throw new Error(
        "benchmark bundle must produce exactly one JavaScript output",
      );
    }

    for (const input of Object.keys(result.metafile.inputs)) {
      const absolute = path.resolve(this.root, input);
      this.sourceFiles.add(
        this.resolveFile(absolute, "bundled module"),
      );
      this.recordResolutionFiles(path.dirname(absolute));
    }
    return outputs[0].text;
  }

  browserModule(filePath: string): string {
    return javascript(filePath);
  }

  private recordResolutionFiles(directory: string): void {
    for (
      let current = canonical(directory);
      isWithin(this.root, current);
      current = path.dirname(current)
    ) {
      for (const name of resolutionFiles) {
        const candidate = path.join(current, name);
        if (fs.statSync(candidate, { throwIfNoEntry: false })?.isFile()) {
          this.sourceFiles.add(canonical(candidate));
        }
      }
      if (current === this.root) {
        break;
      }
    }
  }
}

export function openBrowserProject(
  root: string,
  options: BrowserProjectOptions = {},
): BrowserProject {
  return new ProjectModules(root, options);
}
