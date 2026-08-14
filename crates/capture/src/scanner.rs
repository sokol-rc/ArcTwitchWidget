use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::net::{IpAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use arc_live_core::redaction::{fingerprint, json_shape};
use compact_str::CompactString;
use crossbeam_channel::{Receiver, Sender, bounded};
use pcapsql_core::protocol::{FieldValue, OwnedFieldValue};
use pcapsql_core::schema::{DataKind, FieldDescriptor};
use pcapsql_core::stream::{
    Direction, ParsedMessage, StreamConfig, StreamContext, StreamManager, StreamParseResult,
    StreamParser,
};
use pcapsql_core::tls::KeyLog;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::packet::{TcpSegment, parse_ipv4_tcp};
use crate::raw::{RawCapture, RawCaptureControl};

const MAX_SCAN_BUFFER: usize = 128 * 1024;
const MAX_DISCOVERY_CONNECTIONS: usize = 256;
const MAX_PENDING_REQUESTS: usize = 16;
const DISCOVERY_CONNECTION_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_DECODED_BODY: u64 = 4 * 1024 * 1024;
const MAX_KEYLOG_WINDOW: u64 = 4 * 1024 * 1024;
const MAX_CLIENT_RANDOMS: usize = 2_048;
type StreamBuffers = Arc<Mutex<HashMap<(u64, Direction), Vec<u8>>>>;
type PendingRequests = Arc<Mutex<HashMap<u64, VecDeque<(String, String)>>>>;
const EMBARK_HOSTS: &[&str] = &[
    "api-gateway.europe.es-pio.net",
    "client2pubsub.europe.es-pio.net",
    "client2pubsub-ipv4.europe.es-pio.net",
];

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureStats {
    pub packets_seen: u64,
    pub tcp_443_segments: u64,
    pub tcp_443_to_server: u64,
    pub tcp_443_to_client: u64,
    pub keylog_entries: usize,
    pub keylog_reloads: u64,
    pub tls_records: u64,
    pub tls_records_to_server: u64,
    pub tls_records_to_client: u64,
    pub tls_client_hellos: u64,
    pub tls_server_hellos: u64,
    pub tls_keys_established: u64,
    pub tls_client_hellos_with_keys: u64,
    pub tls_key_errors: u64,
    pub tls_decrypt_errors: u64,
    pub last_tls_sni: Option<String>,
    pub last_embark_sni: Option<String>,
    pub decrypted_records: u64,
    pub observations: u64,
    pub tokens_seen: u64,
    pub active_connections: usize,
    pub buffered_bytes: usize,
    pub connections_evicted: u64,
    pub last_host: Option<String>,
    pub last_path: Option<String>,
}

#[derive(Debug, Clone)]
pub enum CaptureEvent {
    Status(String),
    Stats(Box<CaptureStats>),
    Observation(Value),
    Token {
        token: String,
        fingerprint: String,
    },
    StatsRequestTemplate {
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    Error(String),
    Stopped,
}

pub struct CaptureHandle {
    pub events: Receiver<CaptureEvent>,
    stop: Arc<AtomicBool>,
    control: Arc<Mutex<Option<RawCaptureControl>>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl CaptureHandle {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(control) = self
            .control
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().cloned())
        {
            control.shutdown();
        }
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        self.stop();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub fn start_capture(keylog_path: PathBuf) -> CaptureHandle {
    let (tx, events) = bounded(512);
    let stop = Arc::new(AtomicBool::new(false));
    let control = Arc::new(Mutex::new(None));
    let worker_stop = Arc::clone(&stop);
    let worker_control = Arc::clone(&control);
    let worker = thread::spawn(move || {
        if let Err(error) = capture_loop(&keylog_path, worker_stop, worker_control, &tx) {
            let _ = tx.try_send(CaptureEvent::Error(format!("{error:#}")));
        }
        let _ = tx.try_send(CaptureEvent::Stopped);
    });
    CaptureHandle {
        events,
        stop,
        control,
        worker: Some(worker),
    }
}

fn capture_loop(
    keylog_path: &Path,
    stop: Arc<AtomicBool>,
    control: Arc<Mutex<Option<RawCaptureControl>>>,
    tx: &Sender<CaptureEvent>,
) -> Result<()> {
    let (mut capture, capture_control) = RawCapture::open()?;
    *control.lock().expect("capture control poisoned") = Some(capture_control);
    tx.try_send(CaptureEvent::Status(
        "WinDivert bidirectional capture started".into(),
    ))
    .ok();
    let mut stats = CaptureStats::default();
    let mut signature = None;
    let mut manager = build_manager(load_keylog(keylog_path, &mut stats)?);
    let mut frame = 0u64;
    let mut last_key_check = Instant::now();
    let mut last_stats = Instant::now();
    let mut last_cleanup = Instant::now();
    let mut last_dns_refresh = Instant::now();
    let mut token_fingerprints = HashSet::new();
    let mut client_randoms = VecDeque::new();
    let mut embark_ips = resolve_embark_ips();

    while !stop.load(Ordering::Relaxed) {
        if let Some(packet) = capture.next_packet()? {
            frame = frame.wrapping_add(1);
            stats.packets_seen += 1;
            if let Some(segment) = parse_ipv4_tcp(frame, now_micros(), packet) {
                stats.tcp_443_segments += 1;
                if segment.dst_port == 443 {
                    stats.tcp_443_to_server += 1;
                }
                if segment.src_port == 443 {
                    stats.tcp_443_to_client += 1;
                }
                if embark_ips.is_empty()
                    || embark_ips.contains(&segment.src_ip)
                    || embark_ips.contains(&segment.dst_ip)
                {
                    process_segment(
                        &mut manager,
                        &segment,
                        &mut stats,
                        tx,
                        &mut token_fingerprints,
                        &mut client_randoms,
                    );
                }
            }
        } else {
            thread::sleep(Duration::from_millis(5));
        }

        if last_key_check.elapsed() >= Duration::from_secs(1) {
            let next = file_signature(keylog_path);
            if next != signature && next.is_some() {
                signature = next;
                let keylog = load_keylog(keylog_path, &mut stats)?;
                stats.tls_client_hellos_with_keys = client_randoms
                    .iter()
                    .filter(|random| keylog.lookup(random).is_some())
                    .count() as u64;
                manager.update_keylog(keylog);
                stats.keylog_reloads += 1;
                tx.try_send(CaptureEvent::Status(format!(
                    "TLS keylog reloaded ({} entries)",
                    stats.keylog_entries
                )))
                .ok();
            }
            last_key_check = Instant::now();
        }

        if last_cleanup.elapsed() >= Duration::from_secs(5) {
            let removed = manager.cleanup_timeout(now_micros());
            stats.connections_evicted = stats
                .connections_evicted
                .saturating_add(removed.len() as u64);
            last_cleanup = Instant::now();
        }

        if last_dns_refresh.elapsed() >= Duration::from_secs(300) {
            embark_ips.extend(resolve_embark_ips());
            last_dns_refresh = Instant::now();
        }

        if last_stats.elapsed() >= Duration::from_secs(1) {
            stats.active_connections = manager.connections().count();
            stats.buffered_bytes = manager.total_memory();
            tx.try_send(CaptureEvent::Stats(Box::new(stats.clone())))
                .ok();
            last_stats = Instant::now();
        }
    }
    Ok(())
}

fn resolve_embark_ips() -> HashSet<IpAddr> {
    EMBARK_HOSTS
        .iter()
        .flat_map(|host| (*host, 443).to_socket_addrs().into_iter().flatten())
        .map(|address| address.ip())
        .collect()
}

fn build_manager(keylog: KeyLog) -> StreamManager {
    let mut manager = StreamManager::new(StreamConfig {
        max_connection_buffer: 8 * 1024 * 1024,
        max_total_memory: 64 * 1024 * 1024,
        connection_timeout_us: 60_000_000,
    })
    .with_keylog(keylog);
    manager.registry_mut().register(DiscoveryParser::default());
    manager
}

fn process_segment(
    manager: &mut StreamManager,
    segment: &TcpSegment,
    stats: &mut CaptureStats,
    tx: &Sender<CaptureEvent>,
    seen_tokens: &mut HashSet<String>,
    client_randoms: &mut VecDeque<[u8; 32]>,
) {
    let messages = manager.process_segment(
        segment.src_ip,
        segment.dst_ip,
        segment.src_port,
        segment.dst_port,
        segment.seq,
        segment.ack,
        segment.flags,
        &segment.payload,
        segment.frame_number,
        segment.timestamp_us,
    );
    let Ok(messages) = messages else {
        return;
    };
    for message in messages {
        if message.protocol == "tls" {
            stats.tls_records += 1;
            match message.direction {
                Direction::ToServer => stats.tls_records_to_server += 1,
                Direction::ToClient => stats.tls_records_to_client += 1,
            }
            match string_field(&message, "handshake_type").as_deref() {
                Some("ClientHello") => {
                    stats.tls_client_hellos += 1;
                    if let Some(random) = string_field(&message, "client_random")
                        .and_then(|value| hex::decode(value).ok())
                        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
                        && !client_randoms.contains(&random)
                    {
                        if client_randoms.len() >= MAX_CLIENT_RANDOMS {
                            client_randoms.pop_front();
                        }
                        client_randoms.push_back(random);
                    }
                }
                Some("ServerHello") => stats.tls_server_hellos += 1,
                _ => {}
            }
            if matches!(
                message.fields.get("key_established"),
                Some(FieldValue::Bool(true))
            ) {
                stats.tls_keys_established += 1;
            }
            if message.fields.contains_key("key_error") {
                stats.tls_key_errors += 1;
            }
            if message.fields.contains_key("decrypt_error") {
                stats.tls_decrypt_errors += 1;
            }
            if let Some(sni) = string_field(&message, "sni") {
                if is_allowed_host(&sni) {
                    stats.last_embark_sni = Some(sni.clone());
                }
                stats.last_tls_sni = Some(sni);
            }
            if message.fields.contains_key("decrypted_length") {
                stats.decrypted_records += 1;
            }
            continue;
        }
        if message.protocol == "arc_discovery" {
            let kind = string_field(&message, "kind").unwrap_or_default();
            if kind == "token" {
                if let Some(token) = string_field(&message, "token") {
                    let fp = fingerprint(&token);
                    if seen_tokens.len() >= 64 && !seen_tokens.contains(&fp) {
                        seen_tokens.clear();
                    }
                    if seen_tokens.insert(fp.clone()) {
                        stats.tokens_seen += 1;
                        tx.try_send(CaptureEvent::Token {
                            token,
                            fingerprint: fp,
                        })
                        .ok();
                    }
                }
                continue;
            }
            if kind == "stats_request_template" {
                let headers = string_field(&message, "request_headers")
                    .and_then(|value| serde_json::from_str(&value).ok())
                    .unwrap_or_default();
                let body = string_field(&message, "request_body")
                    .map(String::into_bytes)
                    .unwrap_or_default();
                tx.try_send(CaptureEvent::StatsRequestTemplate { headers, body })
                    .ok();
                continue;
            }
            let host = string_field(&message, "host").unwrap_or_default();
            if !is_allowed_host(&host) {
                continue;
            }
            let path = string_field(&message, "path").map(|value| sanitize_path(&value));
            stats.last_host = Some(host.clone());
            stats.last_path = path.clone();
            stats.observations += 1;
            let observation = json!({
                "protocol": string_field(&message, "source").unwrap_or_else(|| "http1".into()),
                "direction": match message.direction { Direction::ToServer => "request", Direction::ToClient => "response" },
                "host": host,
                "method": string_field(&message, "method"),
                "path": path,
                "status": u64_field(&message, "status"),
                "content_type": string_field(&message, "content_type"),
                "body_shape": string_field(&message, "body_shape")
                    .and_then(|value| serde_json::from_str::<Value>(&value).ok())
                    .unwrap_or(Value::Null),
            });
            tx.try_send(CaptureEvent::Observation(observation)).ok();
        }
    }
}

fn load_keylog(path: &Path, stats: &mut CaptureStats) -> Result<KeyLog> {
    if !path.exists() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, b"").with_context(|| format!("creating {}", path.display()))?;
    }
    let mut file = fs::File::open(path)?;
    let length = file.metadata()?.len();
    let start = length.saturating_sub(MAX_KEYLOG_WINDOW);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity((length - start) as usize);
    file.read_to_end(&mut bytes)?;
    if start > 0 {
        if let Some(first_newline) = bytes.iter().position(|byte| *byte == b'\n') {
            bytes.drain(..=first_newline);
        } else {
            bytes.clear();
        }
    }
    let keylog = KeyLog::from_reader_lenient(bytes.as_slice())?;
    stats.keylog_entries = keylog.entry_count();
    Ok(keylog)
}

fn file_signature(path: &Path) -> Option<(u64, Option<SystemTime>)> {
    let metadata = fs::metadata(path).ok()?;
    Some((metadata.len(), metadata.modified().ok()))
}

fn now_micros() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_micros() as i64)
        .unwrap_or(0)
}

fn string_field(message: &ParsedMessage, key: &str) -> Option<String> {
    message.fields.get(key)?.as_string()
}

fn u64_field(message: &ParsedMessage, key: &str) -> Option<u64> {
    message.fields.get(key)?.as_u64()
}

#[derive(Default)]
struct DiscoveryParser {
    buffers: StreamBuffers,
    hosts: Arc<Mutex<HashMap<u64, String>>>,
    pending_requests: PendingRequests,
    last_seen: Arc<Mutex<HashMap<u64, Instant>>>,
    parse_count: Arc<AtomicU64>,
}

impl DiscoveryParser {
    fn touch_and_prune(&self, connection_id: u64) {
        let now = Instant::now();
        let count = self.parse_count.fetch_add(1, Ordering::Relaxed);
        let victims = {
            let mut last_seen = self.last_seen.lock().expect("last seen map poisoned");
            last_seen.insert(connection_id, now);
            if !count.is_multiple_of(128) && last_seen.len() <= MAX_DISCOVERY_CONNECTIONS {
                return;
            }

            let mut victims: HashSet<u64> = last_seen
                .iter()
                .filter_map(|(id, seen_at)| {
                    (now.saturating_duration_since(*seen_at) > DISCOVERY_CONNECTION_TIMEOUT)
                        .then_some(*id)
                })
                .collect();
            last_seen.retain(|id, _| !victims.contains(id));

            if last_seen.len() > MAX_DISCOVERY_CONNECTIONS {
                let mut oldest: Vec<_> = last_seen
                    .iter()
                    .map(|(id, seen_at)| (*id, *seen_at))
                    .collect();
                oldest.sort_unstable_by_key(|(_, seen_at)| *seen_at);
                for (id, _) in oldest
                    .into_iter()
                    .take(last_seen.len() - MAX_DISCOVERY_CONNECTIONS)
                {
                    last_seen.remove(&id);
                    victims.insert(id);
                }
            }
            victims
        };

        if victims.is_empty() {
            return;
        }
        self.buffers
            .lock()
            .expect("discovery buffers poisoned")
            .retain(|(id, _), _| !victims.contains(id));
        self.hosts
            .lock()
            .expect("discovery hosts poisoned")
            .retain(|id, _| !victims.contains(id));
        self.pending_requests
            .lock()
            .expect("pending requests poisoned")
            .retain(|id, _| !victims.contains(id));
    }
}

impl StreamParser for DiscoveryParser {
    fn name(&self) -> &'static str {
        "http2"
    }
    fn display_name(&self) -> &'static str {
        "ARC Live discovery scanner"
    }
    fn can_parse_stream(&self, _context: &StreamContext) -> bool {
        true
    }

    fn parse_stream(&self, data: &[u8], context: &StreamContext) -> StreamParseResult {
        self.touch_and_prune(context.connection_id);
        let key = (context.connection_id, context.direction);
        let mut all = self.buffers.lock().expect("discovery buffers poisoned");
        let buffer = all.entry(key).or_default();
        buffer.extend_from_slice(data);
        if buffer.len() > MAX_SCAN_BUFFER {
            let drain = buffer.len() - MAX_SCAN_BUFFER;
            buffer.drain(..drain);
        }

        let mut messages = Vec::new();
        while let Some(header_end) = find(buffer, b"\r\n\r\n") {
            let total_headers = header_end + 4;
            let header_text = match std::str::from_utf8(&buffer[..header_end]) {
                Ok(value) => value.to_owned(),
                Err(_) => {
                    buffer.drain(..total_headers);
                    continue;
                }
            };
            let mut lines = header_text.split("\r\n");
            let first = lines.next().unwrap_or_default();
            let mut headers = HashMap::<String, String>::new();
            for line in lines {
                if let Some((name, value)) = line.split_once(':') {
                    headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
                }
            }
            let (body, message_len) = if headers
                .get("transfer-encoding")
                .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"))
            {
                let Some((body, consumed)) = decode_chunked(&buffer[total_headers..]) else {
                    break;
                };
                (body, total_headers + consumed)
            } else {
                let content_len = headers
                    .get("content-length")
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(0);
                if buffer.len() < total_headers + content_len {
                    break;
                }
                (
                    buffer[total_headers..total_headers + content_len].to_vec(),
                    total_headers + content_len,
                )
            };
            buffer.drain(..message_len);

            let (mut method, mut path, status) = parse_start_line(first);
            let mut host = headers.get("host").cloned().unwrap_or_default();
            if !host.is_empty() && is_allowed_host(&host) {
                self.hosts
                    .lock()
                    .expect("discovery hosts poisoned")
                    .insert(context.connection_id, host.clone());
            } else if status.is_some() {
                host = self
                    .hosts
                    .lock()
                    .expect("discovery hosts poisoned")
                    .get(&context.connection_id)
                    .cloned()
                    .unwrap_or_default();
            }
            if !is_allowed_host(&host) {
                continue;
            }

            if let (Some(request_method), Some(request_path)) = (&method, &path) {
                let mut pending = self
                    .pending_requests
                    .lock()
                    .expect("pending requests poisoned");
                let queue = pending.entry(context.connection_id).or_default();
                if queue.len() >= MAX_PENDING_REQUESTS {
                    queue.pop_front();
                }
                queue.push_back((request_method.clone(), request_path.clone()));
            } else if status.is_some()
                && let Some((request_method, request_path)) = self
                    .pending_requests
                    .lock()
                    .expect("pending requests poisoned")
                    .entry(context.connection_id)
                    .or_default()
                    .pop_front()
            {
                method = Some(request_method);
                path = Some(request_path);
            }
            if let Some(token) = headers.get("authorization").and_then(|v| bearer(v)) {
                messages.push(discovery_message(
                    context,
                    "token",
                    &host,
                    method.as_deref(),
                    path.as_deref(),
                    status,
                    headers.get("content-type").map(String::as_str),
                    Value::Null,
                    Some(token),
                    None,
                    None,
                ));
            }
            if status.is_none()
                && method.as_deref() == Some("POST")
                && path.as_deref().is_some_and(|value| {
                    value.split('?').next() == Some("/v1/pioneer/stats/player-v2")
                })
            {
                let replay_headers = replayable_headers(&headers);
                if let Ok(serialized_headers) = serde_json::to_string(&replay_headers) {
                    messages.push(discovery_message(
                        context,
                        "stats_request_template",
                        &host,
                        method.as_deref(),
                        path.as_deref(),
                        status,
                        headers.get("content-type").map(String::as_str),
                        Value::Null,
                        None,
                        Some(&serialized_headers),
                        std::str::from_utf8(&body).ok(),
                    ));
                }
            }
            let decoded_body = decode_content(&body, headers.get("content-encoding"));
            let shape = decoded_body
                .as_deref()
                .and_then(|body| serde_json::from_slice::<Value>(body).ok())
                .map(|value| json_shape(&value, 0))
                .unwrap_or(Value::Null);
            messages.push(discovery_message(
                context,
                "observation",
                &host,
                method.as_deref(),
                path.as_deref(),
                status,
                headers.get("content-type").map(String::as_str),
                shape,
                None,
                None,
                None,
            ));
        }
        StreamParseResult::Complete {
            messages,
            bytes_consumed: data.len(),
        }
    }

    fn message_schema(&self) -> Vec<FieldDescriptor> {
        vec![
            FieldDescriptor::new("kind", DataKind::String),
            FieldDescriptor::new("host", DataKind::String),
            FieldDescriptor::new("method", DataKind::String).set_nullable(true),
            FieldDescriptor::new("path", DataKind::String).set_nullable(true),
            FieldDescriptor::new("status", DataKind::UInt64).set_nullable(true),
            FieldDescriptor::new("content_type", DataKind::String).set_nullable(true),
            FieldDescriptor::new("body_shape", DataKind::String).set_nullable(true),
            FieldDescriptor::new("token", DataKind::String).set_nullable(true),
            FieldDescriptor::new("request_headers", DataKind::String).set_nullable(true),
            FieldDescriptor::new("request_body", DataKind::String).set_nullable(true),
        ]
    }
}

#[allow(clippy::too_many_arguments)]
fn discovery_message(
    context: &StreamContext,
    kind: &str,
    host: &str,
    method: Option<&str>,
    path: Option<&str>,
    status: Option<u64>,
    content_type: Option<&str>,
    shape: Value,
    token: Option<String>,
    request_headers: Option<&str>,
    request_body: Option<&str>,
) -> ParsedMessage {
    let mut fields = HashMap::new();
    put(&mut fields, "kind", kind);
    put(&mut fields, "host", host);
    put(&mut fields, "source", "http1");
    if let Some(value) = method {
        put(&mut fields, "method", value);
    }
    if let Some(value) = path {
        put(&mut fields, "path", value);
    }
    if let Some(value) = status {
        fields.insert("status", FieldValue::UInt64(value));
    }
    if let Some(value) = content_type {
        put(&mut fields, "content_type", value);
    }
    if !shape.is_null() {
        put(&mut fields, "body_shape", &shape.to_string());
    }
    if let Some(value) = token {
        put(&mut fields, "token", &value);
    }
    if let Some(value) = request_headers {
        put(&mut fields, "request_headers", value);
    }
    if let Some(value) = request_body {
        put(&mut fields, "request_body", value);
    }
    ParsedMessage {
        protocol: "arc_discovery",
        connection_id: context.connection_id,
        message_id: 0,
        direction: context.direction,
        frame_number: 0,
        fields,
    }
}

fn replayable_headers(headers: &HashMap<String, String>) -> Vec<(String, String)> {
    const EXCLUDED: &[&str] = &[
        "authorization",
        "connection",
        "content-length",
        "expect",
        "host",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "accept-encoding",
    ];
    headers
        .iter()
        .filter(|(name, _)| !EXCLUDED.contains(&name.as_str()))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

fn put(fields: &mut HashMap<&'static str, OwnedFieldValue>, key: &'static str, value: &str) {
    fields.insert(key, FieldValue::OwnedString(CompactString::new(value)));
}

fn parse_start_line(line: &str) -> (Option<String>, Option<String>, Option<u64>) {
    if line.starts_with("HTTP/") {
        let status = line
            .split_whitespace()
            .nth(1)
            .and_then(|value| value.parse().ok());
        (None, None, status)
    } else {
        let mut parts = line.split_whitespace();
        (
            parts.next().map(str::to_owned),
            parts.next().map(str::to_owned),
            None,
        )
    }
}

fn bearer(value: &str) -> Option<String> {
    let (scheme, token) = value.split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer") && token.matches('.').count() == 2)
        .then(|| token.trim().to_owned())
}

fn is_allowed_host(host: &str) -> bool {
    let normalized = host
        .trim()
        .trim_end_matches('.')
        .split(':')
        .next()
        .unwrap_or(host);
    EMBARK_HOSTS
        .iter()
        .any(|known| normalized.eq_ignore_ascii_case(known))
}

fn sanitize_path(path: &str) -> String {
    let without_query = path.split(['?', '#']).next().unwrap_or(path);
    let mut safe_segments = Vec::new();
    for segment in without_query
        .split('/')
        .filter(|segment| !segment.is_empty())
    {
        let looks_identifier = segment.len() >= 16
            && segment
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_');
        if looks_identifier {
            safe_segments.push(":id");
        } else {
            safe_segments.push(segment);
        }
    }
    let joined = safe_segments.join("/");
    if without_query.starts_with('/') {
        format!("/{joined}")
    } else if joined.is_empty() {
        "/".to_owned()
    } else {
        joined
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn decode_chunked(input: &[u8]) -> Option<(Vec<u8>, usize)> {
    let mut cursor = 0usize;
    let mut output = Vec::new();
    loop {
        let line_end = find(&input[cursor..], b"\r\n")? + cursor;
        let size_text = std::str::from_utf8(&input[cursor..line_end]).ok()?;
        let size = usize::from_str_radix(size_text.split(';').next()?.trim(), 16).ok()?;
        cursor = line_end + 2;
        if size == 0 {
            if input.get(cursor..cursor + 2) == Some(b"\r\n") {
                return Some((output, cursor + 2));
            }
            let trailers_end = find(&input[cursor..], b"\r\n\r\n")? + cursor;
            return Some((output, trailers_end + 4));
        }
        if output.len().saturating_add(size) > MAX_DECODED_BODY as usize
            || input.len() < cursor.saturating_add(size).saturating_add(2)
        {
            return None;
        }
        output.extend_from_slice(&input[cursor..cursor + size]);
        cursor += size;
        if input.get(cursor..cursor + 2) != Some(b"\r\n") {
            return None;
        }
        cursor += 2;
    }
}

fn decode_content(body: &[u8], encoding: Option<&String>) -> Option<Vec<u8>> {
    let encoding = encoding.map(|value| value.trim().to_ascii_lowercase());
    let reader: Box<dyn Read> = match encoding.as_deref() {
        None | Some("") | Some("identity") => Box::new(body),
        Some("gzip") | Some("x-gzip") => Box::new(flate2::read::GzDecoder::new(body)),
        Some("deflate") => Box::new(flate2::read::ZlibDecoder::new(body)),
        Some("br") => Box::new(brotli::Decompressor::new(body, 4096)),
        Some(_) => return None,
    };
    let mut output = Vec::new();
    reader
        .take(MAX_DECODED_BODY + 1)
        .read_to_end(&mut output)
        .ok()?;
    (output.len() <= MAX_DECODED_BODY as usize).then_some(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_rejects_suffix_tricks() {
        assert!(is_allowed_host("api-gateway.europe.es-pio.net:443"));
        assert!(!is_allowed_host("api-gateway.europe.es-pio.net.evil.test"));
    }

    #[test]
    fn json_shape_drops_values() {
        let shape = json_shape(
            &json!({"round_id":"secret-id", "kills": 4, "rows":[{"name":"x"}]}),
            0,
        );
        assert_eq!(shape["round_id"], "string");
        assert_eq!(shape["kills"], "number");
        assert_eq!(shape["rows"]["sample"]["name"], "string");
    }

    #[test]
    fn paths_drop_queries_and_probable_identifiers() {
        assert_eq!(sanitize_path("/rounds?user=secret&token=x"), "/rounds");
        assert_eq!(
            sanitize_path("/users/550e8400-e29b-41d4-a716-446655440000/rounds"),
            "/users/:id/rounds"
        );
    }

    #[test]
    fn decodes_chunked_body_and_trailers() {
        let encoded = b"4\r\nWiki\r\n5;ext=x\r\npedia\r\n0\r\nX-Test: yes\r\n\r\nnext";
        let (decoded, consumed) = decode_chunked(encoded).unwrap();
        assert_eq!(decoded, b"Wikipedia");
        assert_eq!(&encoded[consumed..], b"next");
    }

    #[test]
    fn decodes_gzip_json() {
        use std::io::Write;

        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(br#"{"kills":4}"#).unwrap();
        let compressed = encoder.finish().unwrap();
        assert_eq!(
            decode_content(&compressed, Some(&"gzip".to_owned())).unwrap(),
            br#"{"kills":4}"#
        );
    }

    #[test]
    fn request_replay_keeps_context_but_rebuilds_transport_headers() {
        let headers = HashMap::from([
            ("authorization".to_owned(), "Bearer secret".to_owned()),
            ("content-length".to_owned(), "13".to_owned()),
            ("host".to_owned(), "api.example".to_owned()),
            ("cookie".to_owned(), "session=context".to_owned()),
            ("x-game-context".to_owned(), "required".to_owned()),
        ]);
        let replay = replayable_headers(&headers);

        assert!(!replay.iter().any(|(name, _)| name == "authorization"));
        assert!(!replay.iter().any(|(name, _)| name == "content-length"));
        assert!(!replay.iter().any(|(name, _)| name == "host"));
        assert!(replay.iter().any(|(name, _)| name == "cookie"));
        assert!(replay.iter().any(|(name, _)| name == "x-game-context"));
    }

    #[test]
    fn discovery_state_is_bounded_across_many_connections() {
        let parser = DiscoveryParser::default();
        for connection_id in 1..=(MAX_DISCOVERY_CONNECTIONS as u64 + 100) {
            let context = StreamContext {
                connection_id,
                direction: Direction::ToServer,
                src_ip: "127.0.0.1".parse().unwrap(),
                dst_ip: "127.0.0.2".parse().unwrap(),
                src_port: 50_000,
                dst_port: 443,
                bytes_parsed: 0,
                messages_parsed: 0,
                alpn: None,
            };
            parser.parse_stream(b"incomplete", &context);
        }
        assert!(
            parser
                .last_seen
                .lock()
                .expect("last seen map poisoned")
                .len()
                <= MAX_DISCOVERY_CONNECTIONS
        );
        assert!(
            parser
                .buffers
                .lock()
                .expect("discovery buffers poisoned")
                .len()
                <= MAX_DISCOVERY_CONNECTIONS
        );
    }
}
