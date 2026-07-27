//! Browser-side benchmark harness and network policy shared by Rust adapters.

use std::net::IpAddr;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::lab::{
    BrowserTrialConfig, RuntimeAnchorEvidence, TrialBatchConfig, Viewport, WorkloadEvidence,
};

pub(crate) const VERSION: u32 = 1;
pub(crate) const RUNTIME_ANCHOR_EXPRESSION: &str = "globalThis.__bperfHarness.runtimeAnchor()";
pub(crate) const DOCTOR_PROBE_EXPRESSION: &str = "globalThis.__bperfHarness.doctorProbe()";
pub(crate) const SETTLE_EXPRESSION: &str =
    "(async () => await globalThis.__bperfHarness.settle())()";
pub(crate) const WORKLOAD_READY_EXPRESSION: &str =
    "typeof globalThis.__bperf?.run === \"function\"";
pub(crate) const BENCHMARK_READY_EXPRESSION: &str =
    "Boolean(globalThis.__bperfDescription && globalThis.__bperf)";
pub(crate) const BENCHMARK_DESCRIPTION_EXPRESSION: &str = "globalThis.__bperfDescription";

const SOURCE: &str = include_str!("../../../sidecar/src/browser-workload.js");

pub(crate) fn bootstrap_source() -> String {
    format!(
        r#"{SOURCE}
(() => {{
  const NativeWebSocket = globalThis.WebSocket;
  if (typeof NativeWebSocket !== "function" || NativeWebSocket.__bperfGuarded) return;
  function allowed(value) {{
    const url = new URL(String(value), location.href);
    const octets = url.hostname.split(".");
    const ipv4Loopback = octets.length === 4 && octets[0] === "127" &&
      octets.every((octet) => /^\d{{1,3}}$/.test(octet) && Number(octet) <= 255);
    return (url.protocol === "ws:" || url.protocol === "wss:") &&
      (url.hostname === "localhost" || url.hostname === "::1" ||
       url.hostname === "[::1]" || ipv4Loopback);
  }}
  class BperfWebSocket extends NativeWebSocket {{
    static __bperfGuarded = true;
    constructor(url, protocols) {{
      if (!allowed(url)) {{
        throw new DOMException("Blocked by bperf local-only policy", "SecurityError");
      }}
      if (protocols === undefined) super(url);
      else super(url, protocols);
    }}
  }}
  Object.defineProperties(BperfWebSocket, {{
    CONNECTING: {{ value: NativeWebSocket.CONNECTING }},
    OPEN: {{ value: NativeWebSocket.OPEN }},
    CLOSING: {{ value: NativeWebSocket.CLOSING }},
    CLOSED: {{ value: NativeWebSocket.CLOSED }},
  }});
  globalThis.WebSocket = BperfWebSocket;
}})();"#
    )
}

pub(crate) fn installed_expression() -> String {
    format!("globalThis.__bperfHarness?.version === {VERSION}")
}

pub(crate) struct WorkloadScript {
    operations: String,
}

impl WorkloadScript {
    pub(crate) fn new(operations: &[Value]) -> Result<Self> {
        Ok(Self {
            operations: serde_json::to_string(operations)
                .context("failed to encode browser workload operations")?,
        })
    }

    pub(crate) fn prepare(&self) -> String {
        format!(
            "(async () => await globalThis.__bperfHarness.prepare({}))()",
            self.operations
        )
    }

    pub(crate) fn select_batch_size(&self, batches: TrialBatchConfig) -> Result<String> {
        let target = batches.target_ms().map(Value::from).unwrap_or(Value::Null);
        Ok(format!(
            "(async () => await globalThis.__bperfHarness.selectBatchSize({}, {}, {}, {}))()",
            self.operations,
            batches.initial_size(),
            serde_json::to_string(&target)?,
            batches.max_size(),
        ))
    }

    pub(crate) fn execute(&self, batch_size: u32) -> String {
        format!(
            "(async () => await globalThis.__bperfHarness.execute({}, {batch_size}))()",
            self.operations
        )
    }

    pub(crate) fn inspect_result(&self) -> String {
        format!(
            "(async () => {{ await globalThis.__bperfHarness.prepare({0}); \
             const evidence = await globalThis.__bperfHarness.execute({0}, 1); \
             await globalThis.__bperfHarness.settle(); return evidence.result[0]; }})()",
            self.operations
        )
    }
}

pub(crate) fn decode_runtime_anchor(value: Value) -> Result<RuntimeAnchorEvidence> {
    serde_json::from_value(value).context("browser runtime anchor returned invalid evidence")
}

pub(crate) fn decode_batch_size(value: Value) -> Result<u32> {
    value
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0)
        .context("browser workload returned an invalid batch size")
}

pub(crate) fn decode_workload(value: Value) -> Result<WorkloadEvidence> {
    serde_json::from_value(value).context("browser returned invalid workload evidence")
}

pub(crate) fn default_browser_config() -> BrowserTrialConfig {
    BrowserTrialConfig {
        viewport: Viewport {
            width: 1440,
            height: 900,
        },
        locale: "en-US".to_owned(),
        timezone_id: "UTC".to_owned(),
        color_scheme: "light".to_owned(),
    }
}

pub(crate) fn is_allowed_adapter_url(value: &str) -> bool {
    split_url(value)
        .is_some_and(|(scheme, host)| scheme.eq_ignore_ascii_case("http") && is_loopback_host(host))
}

pub(crate) fn is_allowed_trial_url(value: &str) -> bool {
    if ["data:", "blob:", "about:"]
        .iter()
        .any(|prefix| value.starts_with(prefix))
    {
        return true;
    }
    split_url(value).is_some_and(|(scheme, host)| {
        ["http", "https", "ws", "wss"]
            .iter()
            .any(|allowed| scheme.eq_ignore_ascii_case(allowed))
            && is_loopback_host(host)
    })
}

/// Profiler URLs are benchmark-owned when the same isolated-page network
/// policy would have admitted them. This includes worker blobs, data URLs, and
/// iframe scripts served from a different loopback origin.
pub(crate) fn is_benchmark_code_url(value: &str) -> bool {
    is_allowed_trial_url(value)
}

pub(crate) fn location_contains_benchmark_code(value: &str) -> bool {
    ["http://", "https://", "blob:", "data:", "about:"]
        .iter()
        .flat_map(|marker| value.match_indices(marker).map(|(index, _)| index))
        .any(|index| is_benchmark_code_url(&value[index..]))
}

fn split_url(value: &str) -> Option<(&str, &str)> {
    if value.chars().any(|character| {
        character.is_ascii_control() || character.is_whitespace() || character == '\\'
    }) {
        return None;
    }
    let (scheme, remainder) = value.split_once("://")?;
    if scheme.is_empty() {
        return None;
    }
    let authority = remainder.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let (host, suffix) = bracketed.split_once(']')?;
        let port = if suffix.is_empty() {
            None
        } else {
            Some(suffix.strip_prefix(':')?)
        };
        (host, port)
    } else {
        match authority.split_once(':') {
            Some((host, port)) if !port.contains(':') => (host, Some(port)),
            Some(_) => return None,
            None => (authority, None),
        }
    };
    if host.is_empty() || port.is_some_and(|port| port.is_empty() || port.parse::<u16>().is_err()) {
        return None;
    }
    Some((scheme, host))
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn network_policy_allows_only_loopback_browser_urls() {
        for url in [
            "http://127.0.0.1:1234/",
            "https://localhost/",
            "ws://[::1]:1234/",
            "blob:http://127.0.0.1/id",
            "about:blank",
        ] {
            assert!(is_allowed_trial_url(url), "{url}");
        }
        for url in [
            "https://example.com/",
            "https://127.example.com/",
            "http://evil.example\\@127.0.0.1/",
            "http://user@127.0.0.1/",
            "http://127.0.0.1:invalid/",
            "ws://192.168.1.2/",
            "file:///tmp/a",
            "javascript:alert(1)",
        ] {
            assert!(!is_allowed_trial_url(url), "{url}");
        }
        assert!(is_allowed_adapter_url("http://127.0.0.1:4317/"));
        assert!(!is_allowed_adapter_url("https://127.0.0.1:4317/"));
    }

    #[test]
    fn profiler_attribution_includes_local_frames_workers_and_inline_realms() {
        for location in [
            "http://localhost:4317/frame.js",
            "blob:http://127.0.0.1:4317/worker-id",
            "data:text/javascript,postMessage(1)",
            "about:srcdoc",
            "workerLoop (http://[::1]:4317/worker.js:10:2)",
            "dispatch https://example.com/then http://localhost:4317/frame.js",
        ] {
            assert!(
                location_contains_benchmark_code(location),
                "expected benchmark-owned profiler location: {location}"
            );
        }
        assert!(!location_contains_benchmark_code(
            "dispatch (resource://gre/modules/Timer.sys.mjs:1:1)"
        ));
        assert!(!location_contains_benchmark_code(
            "tracker (https://example.com/worker.js:1:1)"
        ));
    }

    #[test]
    fn workload_script_owns_operation_encoding_and_batch_invocation() {
        let script = WorkloadScript::new(&[json!({
            "case_id": "quotes-are-\"encoded\"",
            "value": 42,
        })])
        .unwrap();

        assert_eq!(
            script.prepare(),
            r#"(async () => await globalThis.__bperfHarness.prepare([{"case_id":"quotes-are-\"encoded\"","value":42}]))()"#
        );
        assert_eq!(
            script
                .select_batch_size(TrialBatchConfig::calibrating(100.0, 1_024))
                .unwrap(),
            r#"(async () => await globalThis.__bperfHarness.selectBatchSize([{"case_id":"quotes-are-\"encoded\"","value":42}], 1, 100.0, 1024))()"#
        );
        assert_eq!(
            script.execute(8),
            r#"(async () => await globalThis.__bperfHarness.execute([{"case_id":"quotes-are-\"encoded\"","value":42}], 8))()"#
        );
    }

    #[test]
    fn workload_decoding_rejects_invalid_batch_sizes() {
        assert_eq!(decode_batch_size(json!(8)).unwrap(), 8);
        for value in [json!(0), json!(-1), json!(1.5), Value::Null] {
            assert!(decode_batch_size(value).is_err());
        }
    }

    #[test]
    fn embedded_workload_source_matches_the_rust_version() {
        assert!(SOURCE.contains(&format!("const VERSION = {VERSION};")));
    }
}
