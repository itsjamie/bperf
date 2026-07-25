export const ENGINE_IDS = ["chromium", "firefox", "webkit"] as const;
export type EngineId = (typeof ENGINE_IDS)[number];

export const ARTIFACT_KINDS = [
  "cpu_profile",
  "js_heap",
  "flamegraph",
] as const;
export type ArtifactKind = (typeof ARTIFACT_KINDS)[number];

export interface RuntimeEvidence {
  node: string;
  playwright: string;
  platform: NodeJS.Platform;
  arch: string;
  os_release: string;
  cpu_model: string;
  logical_cpus: number;
  total_memory_bytes: number;
}

export interface BrowserEvidence {
  root_pid: number;
  executable_path: string;
  version: string;
  launch_args: string[];
}

export interface ArtifactEvidence {
  kind: ArtifactKind;
  path: string;
  size_bytes: number;
  sha256: string;
  format: string;
}

export const RUNTIME_ANCHOR = {
  workload: "javascript_cpu_v1",
  samples: 31,
  maxBatchSize: 64,
} as const;

export interface RuntimeAnchorEvidence {
  workload: typeof RUNTIME_ANCHOR.workload;
  wall_ms: number[];
  batch_size: number;
  checksum: number;
}

export interface EngineCapture {
  browser: BrowserEvidence;
  anchor: RuntimeAnchorEvidence;
  artifacts: ArtifactEvidence[];
}

export interface CaptureCapabilities {
  isolated_launch: true;
  process_root: true;
  cpu_profile: true;
  js_heap: true;
  flamegraph: true;
}

export interface CaptureEvidence extends EngineCapture {
  engine: EngineId;
  runtime: RuntimeEvidence;
  capabilities: CaptureCapabilities;
}

export type EngineAdapter = (
  artifactDirectory: string,
) => Promise<EngineCapture>;

export interface BrowserTrialConfig {
  viewport: {
    width: number;
    height: number;
  };
  locale: string;
  timezone_id: string;
  color_scheme: "light" | "dark" | "no-preference";
}

export interface WorkloadExecution {
  workload_wall_ms: number;
  variant_call_wall_ms: number;
  batch_wall_ms: number;
  batch_size: number;
  operation_count: number;
  result: unknown[];
}

export interface TrialCapture {
  browser: BrowserEvidence;
  workload: WorkloadExecution;
  cpu_active_ms: number;
  js_heap_live_bytes: number;
  artifacts: ArtifactEvidence[];
}

export interface TrialLane<Capture> {
  capture(request: TrialRequest): Promise<Capture>;
  close(): Promise<void>;
}

export interface TrialEngineAdapter {
  openTrialLane(): Promise<TrialLane<TrialCapture>>;
}

export interface TrialRequest {
  targetUrl: string;
  operations: unknown[];
  artifactDirectory: string;
  browser: BrowserTrialConfig;
  batchSize: number;
  batchTargetMs?: number;
  batchMaxSize: number;
}

export interface TrialEvidence {
  engine: EngineId;
  runtime: RuntimeEvidence;
  browser: BrowserEvidence;
  capture_elapsed_ms: number;
  workload: WorkloadExecution;
  metrics: Record<string, number>;
  artifacts: ArtifactEvidence[];
}
