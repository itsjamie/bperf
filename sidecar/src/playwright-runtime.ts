import os from "node:os";

import type { Browser } from "playwright";
import playwrightPackage from "playwright/package.json" with {
  type: "json",
};

import type {
  BrowserEvidence,
  RuntimeEvidence,
} from "./contract.ts";

interface BrowserProcess {
  pid?: number;
  spawnfile?: string;
  spawnargs?: string[];
}

export interface InspectorSession {
  send<Result = unknown>(
    method: string,
    params?: Record<string, unknown>,
  ): Promise<Result>;
  once<Payload = unknown>(
    event: string,
    listener: (payload: Payload) => void,
  ): void;
}

interface ServerBrowser {
  options?: {
    browserProcess?: {
      process?: BrowserProcess;
    };
  };
  _wkPages?: Map<unknown, { _session?: InspectorSession }>;
}

interface InternalBrowser extends Browser {
  _connection?: {
    toImpl(value: Browser): ServerBrowser;
  };
}

function serverBrowser(browser: Browser): ServerBrowser {
  const implementation = (browser as InternalBrowser)._connection?.toImpl(
    browser,
  );
  if (!implementation) {
    throw new Error(
      "Pinned Playwright internals did not expose the server browser",
    );
  }
  return implementation;
}

export function browserInfo(browser: Browser): BrowserEvidence {
  const child = serverBrowser(browser).options?.browserProcess?.process;
  if (!child?.pid || !child.spawnfile) {
    throw new Error("Playwright did not expose the launched browser process");
  }
  return {
    root_pid: child.pid,
    executable_path: child.spawnfile,
    version: browser.version(),
    launch_args: (child.spawnargs ?? []).slice(1),
  };
}

export function webkitInspectorSession(browser: Browser): InspectorSession {
  const webkitPage = [
    ...(serverBrowser(browser)._wkPages?.values() ?? []),
  ][0];
  if (!webkitPage?._session) {
    throw new Error("WebKit inspector session was unavailable");
  }
  return webkitPage._session;
}

export function runtimeInfo(): RuntimeEvidence {
  const cpus = os.cpus();
  return {
    node: process.version,
    playwright: playwrightPackage.version,
    platform: process.platform,
    arch: process.arch,
    os_release: os.release(),
    cpu_model: cpus[0]?.model ?? "unknown",
    logical_cpus: cpus.length,
    total_memory_bytes: os.totalmem(),
  };
}
