import net from "node:net";
import { gunzipSync } from "node:zlib";

import type { FirefoxHeapSnapshotFiles } from "./firefox-heap-snapshot-files.ts";

type RdpPacket = Record<string, unknown>;

const UNSOLICITED_PACKET_TYPES = new Set([
  "allocations",
  "garbage-collection",
  "profiler-started",
  "profiler-stopped",
  "state-change",
]);

interface RdpRequest extends RdpPacket {
  to: string;
  type: string;
}

interface PacketWaiter {
  resolve(packet: RdpMessage): void;
  reject(error: Error): void;
}

interface RdpBulkPacket {
  bulk: true;
  actor: string;
  type: string;
  data: Buffer;
}

type RdpMessage = RdpPacket | RdpBulkPacket;

function isBulkPacket(message: RdpMessage): message is RdpBulkPacket {
  return "bulk" in message && message.bulk === true;
}

function asError(value: unknown): Error {
  return value instanceof Error ? value : new Error(String(value));
}

function isPacket(value: unknown): value is RdpPacket {
  return typeof value === "object" && value !== null;
}

class RdpClient {
  readonly #socket: net.Socket;
  #buffer = Buffer.alloc(0);
  readonly #packets: RdpMessage[] = [];
  readonly #waiters: PacketWaiter[] = [];
  #failure: Error | undefined;

  constructor(socket: net.Socket) {
    this.#socket = socket;
    socket.on("data", (chunk) => {
      try {
        this.#buffer = Buffer.concat([this.#buffer, chunk]);
        this.#parse();
      } catch (error) {
        this.#fail(asError(error));
      }
    });
    socket.once("error", (error) => this.#fail(error));
    socket.once("close", () => {
      if (this.#waiters.length > 0 && !this.#failure) {
        this.#fail(new Error("Firefox RDP connection closed"));
      }
    });
  }

  #parse(): void {
    while (true) {
      const colon = this.#buffer.indexOf(58);
      if (colon < 0) return;

      const header = this.#buffer.subarray(0, colon).toString("utf8");
      const bulk = /^bulk ([^: ]+) ([^: ]+) (\d+)$/.exec(header);
      const length = Number(bulk?.[3] ?? header);
      if (!Number.isSafeInteger(length) || length < 0) {
        throw new Error("Invalid Firefox RDP packet length");
      }

      const end = colon + 1 + length;
      if (this.#buffer.length < end) return;

      let value: RdpMessage;
      if (bulk) {
        value = {
          bulk: true,
          actor: bulk[1],
          type: bulk[2],
          data: Buffer.from(this.#buffer.subarray(colon + 1, end)),
        };
      } else {
        const parsed: unknown = JSON.parse(
          this.#buffer.subarray(colon + 1, end).toString("utf8"),
        );
        if (!isPacket(parsed)) {
          throw new Error("Firefox RDP returned a non-object packet");
        }
        value = parsed;
      }
      this.#buffer = this.#buffer.subarray(end);

      const waiter = this.#waiters.shift();
      if (waiter) waiter.resolve(value);
      else this.#packets.push(value);
    }
  }

  #fail(error: Error): void {
    if (this.#failure) return;
    this.#failure = error;
    for (const waiter of this.#waiters.splice(0)) {
      waiter.reject(error);
    }
    this.#socket.destroy();
  }

  #nextMessage(timeoutMs = 10_000): Promise<RdpMessage> {
    const packet = this.#packets.shift();
    if (packet) return Promise.resolve(packet);
    if (this.#failure) return Promise.reject(this.#failure);

    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        const index = this.#waiters.indexOf(waiter);
        if (index >= 0) this.#waiters.splice(index, 1);
        reject(new Error("Firefox RDP response timed out"));
      }, timeoutMs);
      const waiter: PacketWaiter = {
        resolve: (response) => {
          clearTimeout(timer);
          resolve(response);
        },
        reject: (error) => {
          clearTimeout(timer);
          reject(error);
        },
      };
      this.#waiters.push(waiter);
    });
  }

  async nextPacket(timeoutMs = 10_000): Promise<RdpPacket> {
    const message = await this.#nextMessage(timeoutMs);
    if (isBulkPacket(message)) {
      throw new Error("Firefox RDP returned unexpected bulk data");
    }
    return message;
  }

  async request<Result = RdpPacket>(packet: RdpRequest): Promise<Result> {
    const payload = JSON.stringify(packet);
    this.#socket.write(`${Buffer.byteLength(payload)}:${payload}`);

    while (true) {
      const response = await this.nextPacket();
      if (response.from !== packet.to) continue;
      if (
        typeof response.type === "string" &&
        UNSOLICITED_PACKET_TYPES.has(response.type)
      ) {
        continue;
      }
      if (response.error) {
        throw new Error(
          `Firefox RDP ${packet.type} failed: ${
            response.message ?? response.error
          }`,
        );
      }
      return response as Result;
    }
  }

  async requestBulk(packet: RdpRequest): Promise<Buffer> {
    const payload = JSON.stringify(packet);
    this.#socket.write(`${Buffer.byteLength(payload)}:${payload}`);

    while (true) {
      const response = await this.#nextMessage();
      if (isBulkPacket(response)) {
        if (response.actor === packet.to) return response.data;
        continue;
      }
      if (response.from !== packet.to) continue;
      if (
        typeof response.type === "string" &&
        UNSOLICITED_PACKET_TYPES.has(response.type)
      ) {
        continue;
      }
      if (response.error) {
        throw new Error(
          `Firefox RDP ${packet.type} failed: ${
            response.message ?? response.error
          }`,
        );
      }
    }
  }

  close(): void {
    this.#socket.destroy();
  }
}

export function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      if (!address || typeof address === "string") {
        server.close();
        reject(new Error("Could not allocate a Firefox RDP port"));
        return;
      }
      server.close((error) =>
        error ? reject(error) : resolve(address.port),
      );
    });
  });
}

async function connectRdp(port: number): Promise<RdpClient> {
  let lastError: Error | undefined;
  for (let attempt = 0; attempt < 50; attempt += 1) {
    try {
      const socket = await new Promise<net.Socket>((resolve, reject) => {
        const candidate = net.connect(port, "127.0.0.1");
        candidate.once("connect", () => resolve(candidate));
        candidate.once("error", reject);
      });
      return new RdpClient(socket);
    } catch (error) {
      lastError = asError(error);
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
  }
  throw lastError ?? new Error("Firefox RDP connection failed");
}

interface TabDescriptor {
  actor: string;
  selected?: boolean;
}

interface TargetFrame {
  memoryActor?: string;
}

function responseValue<Value>(packet: RdpPacket, action: string): Value {
  if (!("value" in packet)) {
    throw new Error(`Firefox RDP ${action} returned no value`);
  }
  return packet.value as Value;
}

export class FirefoxDebugSession {
  readonly #rdp: RdpClient;
  readonly #perfActor: string;
  readonly #heapSnapshots: FirefoxHeapSnapshotFiles;

  private constructor(
    rdp: RdpClient,
    perfActor: string,
    heapSnapshots: FirefoxHeapSnapshotFiles,
  ) {
    this.#rdp = rdp;
    this.#perfActor = perfActor;
    this.#heapSnapshots = heapSnapshots;
  }

  static async connect(
    port: number,
    heapSnapshots: FirefoxHeapSnapshotFiles,
  ): Promise<FirefoxDebugSession> {
    const rdp = await connectRdp(port);
    try {
      const greeting = await rdp.nextPacket();
      if (greeting.applicationType !== "browser") {
        throw new Error("Firefox RDP did not return a browser root");
      }
      await rdp.request({
        to: "root",
        type: "connect",
        frontendVersion: "147.0",
      });
      const root = await rdp.request<{ perfActor?: string }>({
        to: "root",
        type: "getRoot",
      });
      if (typeof root.perfActor !== "string") {
        throw new Error("Firefox root did not expose the profiler actor");
      }
      return new FirefoxDebugSession(
        rdp,
        root.perfActor,
        heapSnapshots,
      );
    } catch (error) {
      rdp.close();
      throw error;
    }
  }

  async startProfiler(): Promise<void> {
    const supported = responseValue<boolean>(
      await this.#rdp.request({
        to: this.#perfActor,
        type: "isSupportedPlatform",
      }),
      "isSupportedPlatform",
    );
    if (!supported) {
      throw new Error("Firefox profiler is unavailable on this platform");
    }
    const active = responseValue<boolean>(
      await this.#rdp.request({
        to: this.#perfActor,
        type: "isActive",
      }),
      "isActive",
    );
    if (active) {
      throw new Error("Firefox profiler was active before the CPU interval");
    }
    const started = responseValue<boolean>(
      await this.#rdp.request({
        to: this.#perfActor,
        type: "startProfiler",
        options: {
          entries: 1_000_000,
          interval: 1,
          features: ["js", "stackwalk", "cpu"],
          threads: [
            "GeckoMain",
            "DOM Worker",
            "Renderer",
            "Compositor",
          ],
        },
      }),
      "startProfiler",
    );
    if (!started) {
      throw new Error("Firefox profiler did not start");
    }
  }

  async captureProfile(): Promise<string> {
    const handle = responseValue<number>(
      await this.#rdp.request({
        to: this.#perfActor,
        type: "startCaptureAndStopProfiler",
      }),
      "startCaptureAndStopProfiler",
    );
    if (!Number.isSafeInteger(handle) || handle <= 0) {
      throw new Error("Firefox profiler returned an invalid capture handle");
    }
    const compressed = await this.#rdp.requestBulk({
      to: this.#perfActor,
      type: "getPreviouslyCapturedProfileDataBulk",
      handle,
    });
    return gunzipSync(compressed).toString("utf8");
  }

  async captureHeap(destinationPath: string): Promise<number> {
    const { tabs } = await this.#rdp.request<{
      tabs: TabDescriptor[];
    }>({
      to: "root",
      type: "listTabs",
    });
    if (!Array.isArray(tabs)) {
      throw new Error("Firefox RDP returned no tab list");
    }

    const descriptor = tabs.find((tab) => tab.selected) ?? tabs[0];
    if (!descriptor || typeof descriptor.actor !== "string") {
      throw new Error("Firefox RDP returned no tab descriptor");
    }
    const { frame } = await this.#rdp.request<{ frame: TargetFrame }>({
      to: descriptor.actor,
      type: "getTarget",
    });
    if (!frame || typeof frame.memoryActor !== "string") {
      throw new Error("Firefox target did not expose a memory actor");
    }
    await this.#rdp.request({ to: frame.memoryActor, type: "attach" });
    await this.#rdp.request({
      to: frame.memoryActor,
      type: "forceGarbageCollection",
    });
    await this.#rdp.request({
      to: frame.memoryActor,
      type: "forceCycleCollection",
    });
    const snapshot = await this.#rdp.request<{ snapshotId: string }>({
      to: frame.memoryActor,
      type: "saveHeapSnapshot",
      boundaries: null,
    });
    if (typeof snapshot.snapshotId !== "string") {
      throw new Error(
        "Firefox MemoryActor returned no heap snapshot ID",
      );
    }
    await this.#rdp.request({
      to: frame.memoryActor,
      type: "detach",
    });
    return this.#heapSnapshots.capture(
      snapshot.snapshotId,
      destinationPath,
    );
  }

  close(): void {
    this.#rdp.close();
  }
}
