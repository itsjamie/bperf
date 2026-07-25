import assert from "node:assert/strict";
import test from "node:test";

import {
  createProtocolHandler,
  handleRequest,
  PROTOCOL_VERSION,
} from "../src/protocol.ts";

test("shutdown is the only terminal request", async () => {
  const result = await handleRequest({
    protocol_version: PROTOCOL_VERSION,
    id: 7,
    method: "shutdown",
    params: {},
  });

  assert.equal(result.terminate, true);
  assert.deepEqual(result.response, {
    protocol_version: PROTOCOL_VERSION,
    id: 7,
    ok: true,
    result: { shutdown: true },
  });
});

test("shutdown closes retained browser lanes before acknowledging", async () => {
  let closes = 0;
  const handle = createProtocolHandler({
    async measureBrowserTrial() {
      throw new Error("not used");
    },
    async close() {
      closes += 1;
    },
  });

  const result = await handle({
    protocol_version: PROTOCOL_VERSION,
    id: 15,
    method: "shutdown",
    params: {},
  });

  assert.equal(result.response.ok, true);
  assert.equal(result.terminate, true);
  assert.equal(closes, 1);
});

test("invalid parameters fail before launching a browser", async () => {
  const result = await handleRequest({
    protocol_version: PROTOCOL_VERSION,
    id: 8,
    method: "doctor",
    params: {
      engine: "chrome",
      artifact_dir: "artifacts",
    },
  });

  assert.equal(result.terminate, false);
  assert.equal(result.response.ok, false);
  if (!result.response.ok) {
    assert.equal(result.response.error.code, "invalid_params");
  }
});

test("incomplete trial configuration fails before launching a browser", async () => {
  const result = await handleRequest({
    protocol_version: PROTOCOL_VERSION,
    id: 10,
    method: "measure_trial",
    params: {
      engine: "webkit",
      artifact_dir: "artifacts",
      target_url: "http://127.0.0.1:8080/",
      operations: [],
      browser: {
        viewport: { width: 1440 },
        locale: "en-US",
        timezone_id: "UTC",
        color_scheme: "light",
      },
    },
  });

  assert.equal(result.terminate, false);
  assert.equal(result.response.ok, false);
  if (!result.response.ok) {
    assert.equal(result.response.error.code, "invalid_params");
  }
});

test("external trial targets fail before launching a browser", async () => {
  const result = await handleRequest({
    protocol_version: PROTOCOL_VERSION,
    id: 11,
    method: "measure_trial",
    params: {
      engine: "chromium",
      artifact_dir: "artifacts",
      target_url: "https://example.com/",
      operations: [{ action: "probe" }],
      batch_size: 1,
      batch_target_ms: null,
      batch_max_size: 1,
      browser: {
        viewport: { width: 1440, height: 900 },
        locale: "en-US",
        timezone_id: "UTC",
        color_scheme: "light",
      },
    },
  });

  assert.equal(result.response.ok, false);
  if (!result.response.ok) {
    assert.equal(result.response.error.code, "invalid_params");
  }
});

test("non-positive batch sizes fail before launching a browser", async () => {
  const result = await handleRequest({
    protocol_version: PROTOCOL_VERSION,
    id: 12,
    method: "measure_trial",
    params: {
      engine: "firefox",
      artifact_dir: "artifacts",
      target_url: "http://127.0.0.1:8080/",
      operations: [{ action: "probe" }],
      batch_size: 0,
      batch_target_ms: 100,
      batch_max_size: 10_000,
      browser: {
        viewport: { width: 1440, height: 900 },
        locale: "en-US",
        timezone_id: "UTC",
        color_scheme: "light",
      },
    },
  });

  assert.equal(result.response.ok, false);
  if (!result.response.ok) {
    assert.equal(result.response.error.code, "invalid_params");
  }
});

test("batch sizes cannot exceed their maximum", async () => {
  const result = await handleRequest({
    protocol_version: PROTOCOL_VERSION,
    id: 13,
    method: "measure_trial",
    params: {
      engine: "webkit",
      artifact_dir: "artifacts",
      target_url: "http://127.0.0.1:8080/",
      operations: [{ action: "probe" }],
      batch_size: 2,
      batch_target_ms: 100,
      batch_max_size: 1,
      browser: {
        viewport: { width: 1440, height: 900 },
        locale: "en-US",
        timezone_id: "UTC",
        color_scheme: "light",
      },
    },
  });

  assert.equal(result.response.ok, false);
  if (!result.response.ok) {
    assert.equal(result.response.error.code, "invalid_params");
  }
});

test("non-positive batch targets fail before launching a browser", async () => {
  const result = await handleRequest({
    protocol_version: PROTOCOL_VERSION,
    id: 14,
    method: "measure_trial",
    params: {
      engine: "firefox",
      artifact_dir: "artifacts",
      target_url: "http://127.0.0.1:8080/",
      operations: [{ action: "probe" }],
      batch_size: 1,
      batch_target_ms: 0,
      batch_max_size: 10_000,
      browser: {
        viewport: { width: 1440, height: 900 },
        locale: "en-US",
        timezone_id: "UTC",
        color_scheme: "light",
      },
    },
  });

  assert.equal(result.response.ok, false);
  if (!result.response.ok) {
    assert.equal(result.response.error.code, "invalid_params");
  }
});

test("unsupported protocol versions preserve a valid request id", async () => {
  const result = await handleRequest({
    protocol_version: PROTOCOL_VERSION + 1,
    id: 9,
    method: "shutdown",
  });

  assert.equal(result.terminate, false);
  assert.equal(result.response.id, 9);
  assert.equal(result.response.ok, false);
  if (!result.response.ok) {
    assert.equal(
      result.response.error.code,
      "protocol_version_mismatch",
    );
  }
});
