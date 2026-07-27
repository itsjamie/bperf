//! Firefox profiler and heap capture through the Remote Debugging Protocol.
//!
//! The browser-control pipe and the diagnostic TCP endpoint are independent
//! Firefox protocols. This module hides RDP framing, actor discovery, Gecko
//! profiler capture, and the temporary-file behavior of `.fxsnapshot` files.

use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use flate2::read::GzDecoder;
use serde_json::{Value, json};

const RDP_TIMEOUT: Duration = Duration::from_secs(10);
const RDP_CONNECT_ATTEMPTS: usize = 50;
const RDP_CONNECT_DELAY: Duration = Duration::from_millis(100);
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(10);
const INVALID_SNAPSHOT_PREFIX: &str = "Firefox emitted an invalid .fxsnapshot: ";
const MAX_PROTOBUF_FIELD_NUMBER: u64 = (1 << 29) - 1;
const MAX_PROTOBUF_MESSAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const NODE_ID_FIELD: u64 = 1;
const NODE_SIZE_FIELD: u64 = 4;

const UNSOLICITED_PACKET_TYPES: [&str; 5] = [
    "allocations",
    "garbage-collection",
    "profiler-started",
    "profiler-stopped",
    "state-change",
];

pub(crate) fn free_port() -> Result<u16> {
    let listener =
        TcpListener::bind(("127.0.0.1", 0)).context("could not allocate a Firefox RDP port")?;
    Ok(listener.local_addr()?.port())
}

pub(crate) struct FirefoxDebugSession {
    rdp: RdpClient,
    perf_actor: String,
}

impl FirefoxDebugSession {
    pub(crate) fn connect(port: u16) -> Result<Self> {
        let mut rdp = RdpClient::connect(port)?;
        let greeting = rdp.next_packet()?;
        if greeting.get("applicationType").and_then(Value::as_str) != Some("browser") {
            bail!("Firefox RDP did not return a browser root");
        }
        rdp.request(json!({
            "to": "root",
            "type": "connect",
            "frontendVersion": "147.0",
        }))?;
        let root = rdp.request(json!({"to": "root", "type": "getRoot"}))?;
        let perf_actor = required_string(&root, "perfActor")
            .context("Firefox root did not expose the profiler actor")?;
        Ok(Self { rdp, perf_actor })
    }

    pub(crate) fn start_profiler(&mut self) -> Result<()> {
        let supported = response_value(
            &self.rdp.request(json!({
                "to": self.perf_actor,
                "type": "isSupportedPlatform",
            }))?,
            "isSupportedPlatform",
        )?
        .as_bool()
        .context("Firefox profiler returned an invalid platform capability")?;
        if !supported {
            bail!("Firefox profiler is unavailable on this platform");
        }
        let active = response_value(
            &self.rdp.request(json!({
                "to": self.perf_actor,
                "type": "isActive",
            }))?,
            "isActive",
        )?
        .as_bool()
        .context("Firefox profiler returned an invalid active state")?;
        if active {
            bail!("Firefox profiler was active before the CPU interval");
        }
        let started = response_value(
            &self.rdp.request(profiler_start_request(&self.perf_actor))?,
            "startProfiler",
        )?
        .as_bool()
        .context("Firefox profiler returned an invalid start result")?;
        if !started {
            bail!("Firefox profiler did not start");
        }
        Ok(())
    }

    pub(crate) fn capture_profile(&mut self) -> Result<String> {
        let captured = self.rdp.request(json!({
            "to": self.perf_actor,
            "type": "startCaptureAndStopProfiler",
        }))?;
        let handle = response_value(&captured, "startCaptureAndStopProfiler")?
            .as_u64()
            .context("Firefox profiler returned an invalid capture handle")?;
        if handle == 0 || handle > MAX_SAFE_INTEGER {
            bail!("Firefox profiler returned an invalid capture handle");
        }
        let compressed = self.rdp.request_bulk(json!({
            "to": self.perf_actor,
            "type": "getPreviouslyCapturedProfileDataBulk",
            "handle": handle,
        }))?;
        let mut source = String::new();
        GzDecoder::new(compressed.as_slice())
            .read_to_string(&mut source)
            .context("Firefox profiler returned invalid gzip data")?;
        serde_json::from_str::<Value>(&source).context("Firefox profiler returned invalid JSON")?;
        Ok(source)
    }

    pub(crate) fn capture_heap(
        &mut self,
        destination: &Path,
        snapshots: &mut FirefoxHeapSnapshotFiles,
    ) -> Result<u64> {
        let listed = self
            .rdp
            .request(json!({"to": "root", "type": "listTabs"}))?;
        let tabs = listed
            .get("tabs")
            .and_then(Value::as_array)
            .context("Firefox RDP returned no tab list")?;
        let descriptor = tabs
            .iter()
            .find(|tab| tab.get("selected").and_then(Value::as_bool) == Some(true))
            .or_else(|| tabs.first())
            .context("Firefox RDP returned no tab descriptor")?;
        let actor = required_string(descriptor, "actor")
            .context("Firefox RDP returned an invalid tab descriptor")?;
        let target = self
            .rdp
            .request(json!({"to": actor, "type": "getTarget"}))?;
        let memory_actor = target
            .get("frame")
            .and_then(|frame| frame.get("memoryActor"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .context("Firefox target did not expose a memory actor")?;

        self.rdp
            .request(json!({"to": memory_actor, "type": "attach"}))?;
        let result = (|| {
            self.rdp.request(json!({
                "to": memory_actor,
                "type": "forceGarbageCollection",
            }))?;
            self.rdp.request(json!({
                "to": memory_actor,
                "type": "forceCycleCollection",
            }))?;
            let snapshot = self.rdp.request(json!({
                "to": memory_actor,
                "type": "saveHeapSnapshot",
                "boundaries": Value::Null,
            }))?;
            let snapshot_id = required_string(&snapshot, "snapshotId")
                .context("Firefox MemoryActor returned no heap snapshot ID")?;
            snapshots.capture(&snapshot_id, destination)
        })();
        let detached = self
            .rdp
            .request(json!({"to": memory_actor, "type": "detach"}))
            .map(|_| ());
        match (result, detached) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(error)) => Err(error.context("failed to detach Firefox MemoryActor")),
            (Err(error), Err(detach)) => Err(error.context(format!(
                "Firefox heap capture also failed to detach its MemoryActor: {detach:#}"
            ))),
        }
    }
}

fn profiler_start_request(perf_actor: &str) -> Value {
    json!({
        "to": perf_actor,
        "type": "startProfiler",
        "entries": 1_000_000,
        "interval": 1,
        "features": ["js", "stackwalk", "cpu"],
        "threads": [
            "GeckoMain",
            "DOM Worker",
            "Renderer",
            "Compositor",
        ],
    })
}

enum RdpMessage {
    Packet(Value),
    Bulk {
        actor: String,
        #[allow(dead_code)]
        packet_type: String,
        data: Vec<u8>,
    },
}

struct RdpClient {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl RdpClient {
    fn connect(port: u16) -> Result<Self> {
        let mut last_error = None;
        for _ in 0..RDP_CONNECT_ATTEMPTS {
            match TcpStream::connect(("127.0.0.1", port)) {
                Ok(stream) => {
                    stream.set_nodelay(true)?;
                    stream.set_read_timeout(Some(RDP_TIMEOUT))?;
                    stream.set_write_timeout(Some(RDP_TIMEOUT))?;
                    let writer = stream
                        .try_clone()
                        .context("failed to clone the Firefox RDP socket")?;
                    return Ok(Self {
                        reader: BufReader::new(stream),
                        writer,
                    });
                }
                Err(error) => {
                    last_error = Some(error);
                    thread::sleep(RDP_CONNECT_DELAY);
                }
            }
        }
        Err(last_error
            .map(anyhow::Error::from)
            .unwrap_or_else(|| anyhow!("Firefox RDP connection failed")))
        .context("Firefox RDP connection failed")
    }

    fn request(&mut self, packet: Value) -> Result<Value> {
        let actor = required_string(&packet, "to")?;
        let packet_type = required_string(&packet, "type")?;
        self.send(&packet)?;
        loop {
            match self.next_message()? {
                RdpMessage::Bulk { .. } => {
                    bail!("Firefox RDP returned unexpected bulk data");
                }
                RdpMessage::Packet(response) => {
                    if response.get("from").and_then(Value::as_str) != Some(actor.as_str()) {
                        continue;
                    }
                    if response
                        .get("type")
                        .and_then(Value::as_str)
                        .is_some_and(|value| UNSOLICITED_PACKET_TYPES.contains(&value))
                    {
                        continue;
                    }
                    check_rdp_error(&response, &packet_type)?;
                    return Ok(response);
                }
            }
        }
    }

    fn request_bulk(&mut self, packet: Value) -> Result<Vec<u8>> {
        let actor = required_string(&packet, "to")?;
        let packet_type = required_string(&packet, "type")?;
        self.send(&packet)?;
        loop {
            match self.next_message()? {
                RdpMessage::Bulk {
                    actor: response_actor,
                    data,
                    ..
                } if response_actor == actor => return Ok(data),
                RdpMessage::Bulk { .. } => {}
                RdpMessage::Packet(response) => {
                    if response.get("from").and_then(Value::as_str) != Some(actor.as_str()) {
                        continue;
                    }
                    if response
                        .get("type")
                        .and_then(Value::as_str)
                        .is_some_and(|value| UNSOLICITED_PACKET_TYPES.contains(&value))
                    {
                        continue;
                    }
                    check_rdp_error(&response, &packet_type)?;
                }
            }
        }
    }

    fn next_packet(&mut self) -> Result<Value> {
        match self.next_message()? {
            RdpMessage::Packet(packet) => Ok(packet),
            RdpMessage::Bulk { .. } => bail!("Firefox RDP returned unexpected bulk data"),
        }
    }

    fn send(&mut self, packet: &Value) -> Result<()> {
        let payload = serde_json::to_vec(packet)?;
        write!(&mut self.writer, "{}:", payload.len())
            .context("failed writing a Firefox RDP packet header")?;
        self.writer
            .write_all(&payload)
            .context("failed writing a Firefox RDP packet")?;
        self.writer
            .flush()
            .context("failed flushing a Firefox RDP packet")
    }

    fn next_message(&mut self) -> Result<RdpMessage> {
        let mut header = Vec::new();
        let bytes = self
            .reader
            .read_until(b':', &mut header)
            .context("failed reading a Firefox RDP packet header")?;
        if bytes == 0 {
            bail!("Firefox RDP connection closed");
        }
        if header.last() != Some(&b':') {
            bail!("Firefox RDP connection closed inside a packet header");
        }
        header.pop();
        let header =
            std::str::from_utf8(&header).context("Firefox RDP packet header was not UTF-8")?;
        let (bulk, length) = parse_rdp_header(header)?;
        let mut data = vec![0; length];
        self.reader
            .read_exact(&mut data)
            .context("Firefox RDP connection closed inside a packet")?;
        if let Some((actor, packet_type)) = bulk {
            return Ok(RdpMessage::Bulk {
                actor,
                packet_type,
                data,
            });
        }
        let packet: Value =
            serde_json::from_slice(&data).context("Firefox RDP returned invalid JSON")?;
        if !packet.is_object() {
            bail!("Firefox RDP returned a non-object packet");
        }
        Ok(RdpMessage::Packet(packet))
    }
}

fn parse_rdp_header(header: &str) -> Result<(Option<(String, String)>, usize)> {
    if let Some(rest) = header.strip_prefix("bulk ") {
        let mut fields = rest.split(' ');
        let actor = fields.next().unwrap_or_default();
        let packet_type = fields.next().unwrap_or_default();
        let length = fields.next().unwrap_or_default();
        if actor.is_empty()
            || packet_type.is_empty()
            || length.is_empty()
            || fields.next().is_some()
        {
            bail!("Invalid Firefox RDP packet length");
        }
        let length = length
            .parse::<usize>()
            .context("Invalid Firefox RDP packet length")?;
        return Ok((Some((actor.to_owned(), packet_type.to_owned())), length));
    }
    let length = header
        .parse::<usize>()
        .context("Invalid Firefox RDP packet length")?;
    Ok((None, length))
}

fn check_rdp_error(packet: &Value, action: &str) -> Result<()> {
    if packet.get("error").is_none_or(Value::is_null) {
        return Ok(());
    }
    let message = packet
        .get("message")
        .or_else(|| packet.get("error"))
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string())
        })
        .unwrap_or_else(|| "unknown RDP error".to_owned());
    bail!("Firefox RDP {action} failed: {message}")
}

fn response_value<'a>(packet: &'a Value, action: &str) -> Result<&'a Value> {
    packet
        .get("value")
        .with_context(|| format!("Firefox RDP {action} returned no value"))
}

fn required_string(value: &Value, field: &str) -> Result<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .with_context(|| format!("Firefox RDP response has no {field}"))
}

#[derive(Default)]
pub(crate) struct FirefoxHeapSnapshotFiles {
    sources: BTreeSet<PathBuf>,
}

impl FirefoxHeapSnapshotFiles {
    fn capture(&mut self, snapshot_id: &str, destination: &Path) -> Result<u64> {
        if !valid_snapshot_id(snapshot_id) {
            bail!("Firefox MemoryActor returned an invalid heap snapshot ID");
        }
        let source = std::env::temp_dir().join(format!("{snapshot_id}.fxsnapshot"));
        self.sources.insert(source.clone());
        wait_for_snapshot(&source)?;
        if destination.exists() {
            fs::remove_file(destination)
                .with_context(|| format!("failed to replace {}", destination.display()))?;
        }
        if let Err(link_error) = fs::hard_link(&source, destination) {
            fs::copy(&source, destination).with_context(|| {
                format!(
                    "failed to retain Firefox heap snapshot {} after hard-link failure: {link_error}",
                    destination.display()
                )
            })?;
        }
        let live_bytes = firefox_heap_snapshot_live_bytes(destination)?;
        if remove_snapshot_if_released(&source)? {
            self.sources.remove(&source);
        }
        Ok(live_bytes)
    }

    pub(crate) fn close(&mut self) -> Result<()> {
        let sources = self.sources.iter().cloned().collect::<Vec<_>>();
        let mut failures = Vec::new();
        for source in sources {
            match remove_snapshot(&source) {
                Ok(()) => {
                    self.sources.remove(&source);
                }
                Err(error) => failures.push(error),
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            let details = failures
                .into_iter()
                .map(|error| format!("{error:#}"))
                .collect::<Vec<_>>()
                .join("; ");
            bail!("Firefox did not release one or more heap snapshots: {details}")
        }
    }
}

fn valid_snapshot_id(value: &str) -> bool {
    let mut parts = value.split('-');
    let first = parts.next().unwrap_or_default();
    let second = parts.next();
    !first.is_empty()
        && first.bytes().all(|byte| byte.is_ascii_digit())
        && second
            .is_none_or(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        && parts.next().is_none()
}

fn wait_for_snapshot(path: &Path) -> Result<()> {
    let deadline = Instant::now() + SNAPSHOT_TIMEOUT;
    loop {
        if fs::metadata(path).is_ok_and(|metadata| metadata.len() > 0) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("Firefox did not write heap snapshot {}", path.display());
        }
        thread::sleep(RDP_CONNECT_DELAY);
    }
}

fn remove_snapshot_if_released(path: &Path) -> Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) if snapshot_is_locked(&error) => Ok(false),
        Err(error) => Err(error)
            .with_context(|| format!("failed to remove Firefox heap snapshot {}", path.display())),
    }
}

#[cfg(windows)]
fn snapshot_is_locked(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::PermissionDenied
        || matches!(error.raw_os_error(), Some(5 | 32 | 33))
}

#[cfg(not(windows))]
fn snapshot_is_locked(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::PermissionDenied
}

fn remove_snapshot(path: &Path) -> Result<()> {
    let deadline = Instant::now() + SNAPSHOT_TIMEOUT;
    loop {
        if remove_snapshot_if_released(path)? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("Firefox did not release heap snapshot {}", path.display());
        }
        thread::sleep(RDP_CONNECT_DELAY);
    }
}

pub(crate) fn firefox_heap_snapshot_live_bytes(path: &Path) -> Result<u64> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut decoder = GzDecoder::new(file);
    let mut messages = HeapSnapshotMessages::default();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = decoder
            .read(&mut buffer)
            .map_err(|error| invalid_snapshot(format!("gzip decoding failed: {error}")))?;
        if count == 0 {
            break;
        }
        messages.consume(&buffer[..count])?;
    }
    messages.finish()
}

#[derive(Default)]
struct HeapSnapshotMessages {
    encoded_length: usize,
    length_byte_count: usize,
    message_length: Option<usize>,
    message: Vec<u8>,
    message_index: usize,
    node_count: usize,
    total: u64,
}

impl HeapSnapshotMessages {
    fn consume(&mut self, chunk: &[u8]) -> Result<()> {
        let mut offset = 0;
        while offset < chunk.len() {
            let Some(length) = self.message_length else {
                self.consume_length_byte(chunk[offset])?;
                offset += 1;
                continue;
            };
            let remaining = length - self.message.len();
            let count = remaining.min(chunk.len() - offset);
            self.message
                .extend_from_slice(&chunk[offset..offset + count]);
            offset += count;
            if self.message.len() == length {
                self.finish_message()?;
            }
        }
        Ok(())
    }

    fn consume_length_byte(&mut self, byte: u8) -> Result<()> {
        if self.length_byte_count == 4 && byte & 0xf0 != 0 {
            return Err(invalid_snapshot(
                "heap snapshot message length exceeds 32 bits",
            ));
        }
        self.encoded_length = self
            .encoded_length
            .checked_add(
                usize::from(byte & 0x7f)
                    .checked_shl((self.length_byte_count * 7) as u32)
                    .context("heap snapshot message length overflowed")?,
            )
            .context("heap snapshot message length overflowed")?;
        self.length_byte_count += 1;
        if byte & 0x80 != 0 {
            if self.length_byte_count == 5 {
                return Err(invalid_snapshot(
                    "heap snapshot message length exceeds 32 bits",
                ));
            }
            return Ok(());
        }
        if self.encoded_length == 0 {
            return Err(invalid_snapshot("heap snapshot message is empty"));
        }
        if self.encoded_length > MAX_PROTOBUF_MESSAGE_BYTES {
            return Err(invalid_snapshot(
                "heap snapshot message exceeds Firefox's protobuf limit",
            ));
        }
        self.message_length = Some(self.encoded_length);
        self.message = Vec::with_capacity(self.encoded_length);
        Ok(())
    }

    fn finish_message(&mut self) -> Result<()> {
        if self.message_index > 0 {
            let size = node_size(&self.message)?;
            self.total = self
                .total
                .checked_add(size)
                .filter(|total| *total <= MAX_SAFE_INTEGER)
                .ok_or_else(|| {
                    invalid_snapshot("total heap size exceeds JavaScript's safe range")
                })?;
            self.node_count += 1;
        }
        self.message_index += 1;
        self.encoded_length = 0;
        self.length_byte_count = 0;
        self.message_length = None;
        self.message.clear();
        Ok(())
    }

    fn finish(self) -> Result<u64> {
        if self.message_length.is_some() || self.length_byte_count > 0 {
            return Err(invalid_snapshot("truncated heap snapshot message"));
        }
        if self.message_index == 0 {
            return Err(invalid_snapshot("heap snapshot contains no metadata"));
        }
        if self.node_count == 0 || self.total == 0 {
            return Err(invalid_snapshot(
                "heap snapshot contains no live heap nodes",
            ));
        }
        Ok(self.total)
    }
}

fn node_size(message: &[u8]) -> Result<u64> {
    let mut offset = 0;
    let mut has_id = false;
    let mut size = None;
    while offset < message.len() {
        let tag = read_tag(message, offset, message.len())?;
        offset = tag.next_offset;
        if tag.field_number == NODE_ID_FIELD && tag.wire_type == 0 {
            let id = read_varint(message, offset, message.len())?;
            has_id = true;
            offset = id.next_offset;
        } else if tag.field_number == NODE_SIZE_FIELD && tag.wire_type == 0 {
            let encoded = read_varint(message, offset, message.len())?;
            size = Some(encoded.value);
            offset = encoded.next_offset;
        } else {
            offset = skip_field(
                message,
                offset,
                message.len(),
                tag.field_number,
                tag.wire_type,
            )?;
        }
    }
    if !has_id {
        return Err(invalid_snapshot("heap node has no ID"));
    }
    let size = size.ok_or_else(|| invalid_snapshot("heap node has no size"))?;
    if size > MAX_SAFE_INTEGER {
        return Err(invalid_snapshot("heap node has an invalid size"));
    }
    Ok(size)
}

struct Varint {
    value: u64,
    next_offset: usize,
}

fn read_varint(buffer: &[u8], mut offset: usize, limit: usize) -> Result<Varint> {
    let mut value = 0_u64;
    for index in 0..10 {
        if offset >= limit {
            return Err(invalid_snapshot("truncated protobuf varint"));
        }
        let byte = buffer[offset];
        offset += 1;
        if index == 9 && byte > 1 {
            return Err(invalid_snapshot("protobuf varint exceeds 64 bits"));
        }
        value |= u64::from(byte & 0x7f) << (index * 7);
        if byte & 0x80 == 0 {
            return Ok(Varint {
                value,
                next_offset: offset,
            });
        }
    }
    Err(invalid_snapshot("protobuf varint exceeds 64 bits"))
}

struct Tag {
    field_number: u64,
    wire_type: u8,
    next_offset: usize,
}

fn read_tag(buffer: &[u8], offset: usize, limit: usize) -> Result<Tag> {
    let tag = read_varint(buffer, offset, limit)?;
    let field_number = tag.value >> 3;
    let wire_type = (tag.value & 0x07) as u8;
    if field_number == 0 || field_number > MAX_PROTOBUF_FIELD_NUMBER {
        return Err(invalid_snapshot("invalid protobuf field number"));
    }
    Ok(Tag {
        field_number,
        wire_type,
        next_offset: tag.next_offset,
    })
}

fn skip_field(
    buffer: &[u8],
    offset: usize,
    limit: usize,
    field_number: u64,
    wire_type: u8,
) -> Result<usize> {
    match wire_type {
        0 => Ok(read_varint(buffer, offset, limit)?.next_offset),
        1 if limit.saturating_sub(offset) >= 8 => Ok(offset + 8),
        1 => Err(invalid_snapshot("truncated fixed64 protobuf field")),
        2 => {
            let encoded = read_varint(buffer, offset, limit)?;
            let length = usize::try_from(encoded.value)
                .map_err(|_| invalid_snapshot("length-delimited protobuf field exceeds range"))?;
            if length > limit.saturating_sub(encoded.next_offset) {
                return Err(invalid_snapshot(
                    "truncated length-delimited protobuf field",
                ));
            }
            Ok(encoded.next_offset + length)
        }
        3 => {
            let mut cursor = offset;
            while cursor < limit {
                let tag = read_tag(buffer, cursor, limit)?;
                cursor = tag.next_offset;
                if tag.wire_type == 4 {
                    if tag.field_number != field_number {
                        return Err(invalid_snapshot("mismatched protobuf group"));
                    }
                    return Ok(cursor);
                }
                cursor = skip_field(buffer, cursor, limit, tag.field_number, tag.wire_type)?;
            }
            Err(invalid_snapshot("unterminated protobuf group"))
        }
        4 => Err(invalid_snapshot("unexpected protobuf end-group")),
        5 if limit.saturating_sub(offset) >= 4 => Ok(offset + 4),
        5 => Err(invalid_snapshot("truncated fixed32 protobuf field")),
        other => Err(invalid_snapshot(format!(
            "unsupported protobuf wire type {other}"
        ))),
    }
}

fn invalid_snapshot(reason: impl std::fmt::Display) -> anyhow::Error {
    anyhow!("{INVALID_SNAPSHOT_PREFIX}{reason}")
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn parses_the_checked_in_firefox_heap_fixture() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("sidecar/test/fixtures/captures/firefox/heap.fxsnapshot");
        assert_eq!(firefox_heap_snapshot_live_bytes(&path).unwrap(), 301);
    }

    #[test]
    fn snapshot_ids_cannot_escape_the_system_temp_directory() {
        for invalid in ["", "../escape", "1/2", "1-", "-1", "1-2-3", "abc"] {
            assert!(!valid_snapshot_id(invalid), "{invalid}");
        }
        for valid in ["1", "123456", "123-456"] {
            assert!(valid_snapshot_id(valid), "{valid}");
        }
    }

    #[test]
    fn rdp_headers_distinguish_json_and_bulk_packets() {
        assert_eq!(parse_rdp_header("12").unwrap(), (None, 12));
        assert_eq!(
            parse_rdp_header("bulk profiler profile 42").unwrap(),
            (Some(("profiler".to_owned(), "profile".to_owned())), 42)
        );
        assert!(parse_rdp_header("bulk actor type nope").is_err());
    }

    #[test]
    fn profiler_options_use_the_actor_wire_shape_and_include_workers() {
        let request = profiler_start_request("profiler");
        assert_eq!(request["to"], "profiler");
        assert_eq!(request["type"], "startProfiler");
        assert_eq!(request["interval"], 1);
        assert!(request.get("options").is_none());
        assert!(
            request["threads"]
                .as_array()
                .unwrap()
                .contains(&json!("DOM Worker"))
        );
    }

    #[test]
    fn node_messages_require_an_id_and_size() {
        assert_eq!(node_size(&[0x08, 0x01, 0x20, 0x50]).unwrap(), 80);
        assert!(node_size(&[0x20, 0x50]).is_err());
        assert!(node_size(&[0x08, 0x01]).is_err());
    }

    #[test]
    fn truncated_gzip_snapshots_fail_explicitly() {
        let source = fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join("sidecar/test/fixtures/captures/firefox/heap.fxsnapshot"),
        )
        .unwrap();
        let directory = tempdir().unwrap();
        let path = directory.path().join("truncated.fxsnapshot");
        fs::write(&path, &source[..source.len() - 2]).unwrap();
        let error = firefox_heap_snapshot_live_bytes(&path)
            .unwrap_err()
            .to_string();
        assert!(error.starts_with(INVALID_SNAPSHOT_PREFIX));
    }
}
