use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use arc_live_core::redaction::json_shape;
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
use crate::source::{PacketSource, SourceControl};

/// How much unframed data is kept while looking for the start of a message.
/// Only applies when no message header is in sight - a message whose length is
/// already known is allowed to finish arriving, up to [`MAX_MESSAGE_BYTES`].
const MAX_SCAN_BUFFER: usize = 128 * 1024;
/// The largest single HTTP message that is assembled. The statistics response
/// grows with the account's history and passed the old 128 KiB scan buffer for
/// long-lived accounts, which silently stopped their capture; anything past
/// this is walked past by length rather than cut out of the stream.
const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_DISCOVERY_CONNECTIONS: usize = 256;
const MAX_PENDING_REQUESTS: usize = 16;
/// How long a silent connection is kept. The game holds one HTTPS connection to
/// its API open across a whole raid and reuses it for the statistics request on
/// the way back to Speranza. Dropping it while the player is in a raid loses the
/// TLS state, so the response that finally arrives cannot be decrypted and the
/// stream counters silently stop. The window therefore has to outlast a raid.
const CONNECTION_IDLE_TIMEOUT: Duration = Duration::from_secs(45 * 60);
const DISCOVERY_CONNECTION_TIMEOUT: Duration = CONNECTION_IDLE_TIMEOUT;
/// Connections to the game's API are never evicted to make room for others.
/// Their number is still bounded, so a long stream cannot grow the map forever.
const MAX_TRACKED_API_CONNECTIONS: usize = 64;
/// The one endpoint ARC Live reads. The game requests it on every return to the
/// lobby; the response is recognised by its body as well as by this path.
const PLAYER_STATS_PATH: &str = "/v1/pioneer/stats/player-v2";
const MAX_DECODED_BODY: u64 = 4 * 1024 * 1024;
const MAX_KEYLOG_WINDOW: u64 = 4 * 1024 * 1024;
const MAX_CLIENT_RANDOMS: usize = 2_048;
type StreamBuffers = Arc<Mutex<HashMap<(u64, Direction), Vec<u8>>>>;
type PendingRequests = Arc<Mutex<HashMap<u64, VecDeque<(String, String)>>>>;
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
    pub regional_api_hosts: Vec<String>,
    pub decrypted_records: u64,
    pub observations: u64,
    pub active_connections: usize,
    pub buffered_bytes: usize,
    pub connections_evicted: u64,
    pub last_host: Option<String>,
    pub last_path: Option<String>,
    /// Which packet source is running: `raw socket` or `WinDivert`.
    pub capture_backend: String,
    /// Segments Winsock delivered truncated and we had to drop.
    pub oversized_packets: u64,
}

#[derive(Debug, Clone)]
pub enum CaptureEvent {
    Status(String),
    Stats(Box<CaptureStats>),
    Observation(Value),
    StatsStreamReady {
        host: String,
    },
    PlayerStatsResponse {
        host: String,
        status: u16,
        content_type: Option<String>,
        body: Value,
    },
    Error(String),
    Stopped,
}

pub struct CaptureHandle {
    pub events: Receiver<CaptureEvent>,
    stop: Arc<AtomicBool>,
    control: Arc<Mutex<Option<SourceControl>>>,
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
    control: Arc<Mutex<Option<SourceControl>>>,
    tx: &Sender<CaptureEvent>,
) -> Result<()> {
    let (mut capture, capture_control, backend, fallback_reason) = PacketSource::open()?;
    *control.lock().expect("capture control poisoned") = Some(capture_control);
    tx.try_send(CaptureEvent::Status(format!(
        "Bidirectional capture started ({})",
        backend.as_str()
    )))
    .ok();
    if let Some(reason) = fallback_reason {
        tx.try_send(CaptureEvent::Status(format!(
            "Raw-socket capture was unavailable, using the driver instead: {reason}"
        )))
        .ok();
    }
    let mut stats = CaptureStats {
        capture_backend: backend.as_str().to_owned(),
        ..Default::default()
    };
    let mut signature = None;
    let (mut manager, api_connections) = build_manager(load_keylog(keylog_path, &mut stats)?);
    let mut frame = 0u64;
    let mut last_key_check = Instant::now();
    let mut last_stats = Instant::now();
    let mut last_cleanup = Instant::now();
    let mut client_randoms = VecDeque::new();
    let mut embark_ips = HashSet::new();

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
                if segment.dst_port == 443
                    && let Some(sni) = tls_client_hello_sni(&segment.payload)
                {
                    stats.last_tls_sni = Some(sni.clone());
                    if looks_like_regional_api_host(&sni)
                        && !stats.regional_api_hosts.contains(&sni)
                    {
                        if stats.regional_api_hosts.len() >= 16 {
                            stats.regional_api_hosts.remove(0);
                        }
                        stats.regional_api_hosts.push(sni.clone());
                    }
                    if is_allowed_host(&sni) {
                        stats.last_embark_sni = Some(sni);
                        embark_ips.insert(segment.dst_ip);
                    }
                }
                if embark_ips.is_empty()
                    || embark_ips.contains(&segment.src_ip)
                    || embark_ips.contains(&segment.dst_ip)
                {
                    process_segment(&mut manager, &segment, &mut stats, tx, &mut client_randoms);
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
            // Losing one of these is the only way a working capture can go
            // quiet mid-stream, so it is reported instead of passing silently.
            if !removed.is_empty() {
                let mut hosts = api_connections.lock().expect("discovery hosts poisoned");
                for connection in &removed {
                    if let Some(host) = hosts.remove(&connection.id) {
                        tx.try_send(CaptureEvent::Status(format!(
                            "Idle connection to {host} timed out; the next statistics response on \
                             it cannot be decrypted"
                        )))
                        .ok();
                    }
                }
            }
            last_cleanup = Instant::now();
        }

        if last_stats.elapsed() >= Duration::from_secs(1) {
            stats.oversized_packets = capture.oversized_packets();
            stats.active_connections = manager.connections().count();
            stats.buffered_bytes = manager.total_memory();
            tx.try_send(CaptureEvent::Stats(Box::new(stats.clone())))
                .ok();
            last_stats = Instant::now();
        }
    }
    Ok(())
}

/// Returns the manager together with the map of connections that talk to the
/// game's API, so the capture loop can tell when one of them is dropped.
fn build_manager(keylog: KeyLog) -> (StreamManager, Arc<Mutex<HashMap<u64, String>>>) {
    let mut manager = StreamManager::new(StreamConfig {
        max_connection_buffer: 8 * 1024 * 1024,
        max_total_memory: 64 * 1024 * 1024,
        connection_timeout_us: CONNECTION_IDLE_TIMEOUT.as_micros() as i64,
    })
    .with_keylog(keylog);
    let parser = DiscoveryParser::default();
    let hosts = Arc::clone(&parser.hosts);
    manager.registry_mut().register(parser);
    (manager, hosts)
}

fn process_segment(
    manager: &mut StreamManager,
    segment: &TcpSegment,
    stats: &mut CaptureStats,
    tx: &Sender<CaptureEvent>,
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
            if kind == "stats_stream_ready" {
                let host = string_field(&message, "host").unwrap_or_default();
                tx.try_send(CaptureEvent::StatsStreamReady { host }).ok();
                continue;
            }
            if kind == "player_stats_response" {
                let host = string_field(&message, "host").unwrap_or_default();
                let status = u64_field(&message, "status").unwrap_or(200) as u16;
                let content_type = string_field(&message, "content_type");
                if let Some(body) = string_field(&message, "stats_body")
                    .and_then(|value| serde_json::from_str(&value).ok())
                {
                    tx.try_send(CaptureEvent::PlayerStatsResponse {
                        host,
                        status,
                        content_type,
                        body,
                    })
                    .ok();
                }
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
    /// Bytes of a body too large to keep that still have to be walked past, so
    /// the stream stays framed instead of being cut in the middle of a message.
    skipped_bodies: Arc<Mutex<HashMap<(u64, Direction), usize>>>,
}

impl DiscoveryParser {
    fn touch_and_prune(&self, connection_id: u64) {
        let now = Instant::now();
        let count = self.parse_count.fetch_add(1, Ordering::Relaxed);
        // The host map only ever holds connections to the game's API, so it is
        // exactly the set that must survive a quiet raid.
        let protected: HashSet<u64> = self
            .hosts
            .lock()
            .expect("discovery hosts poisoned")
            .keys()
            .copied()
            .collect();
        let victims = {
            let mut last_seen = self.last_seen.lock().expect("last seen map poisoned");
            last_seen.insert(connection_id, now);
            if !count.is_multiple_of(128) && last_seen.len() <= MAX_DISCOVERY_CONNECTIONS {
                return;
            }

            let mut victims: HashSet<u64> = last_seen
                .iter()
                .filter_map(|(id, seen_at)| {
                    (!protected.contains(id)
                        && now.saturating_duration_since(*seen_at) > DISCOVERY_CONNECTION_TIMEOUT)
                        .then_some(*id)
                })
                .collect();
            last_seen.retain(|id, _| !victims.contains(id));

            if last_seen.len() > MAX_DISCOVERY_CONNECTIONS {
                let mut oldest: Vec<_> = last_seen
                    .iter()
                    .filter(|(id, _)| !protected.contains(id))
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
        self.skipped_bodies
            .lock()
            .expect("skipped bodies poisoned")
            .retain(|(id, _), _| !victims.contains(id));
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

        // Walk past the remains of a body that was too large to assemble. Doing
        // this by length keeps the next message correctly framed.
        {
            let mut skipped = self.skipped_bodies.lock().expect("skipped bodies poisoned");
            if let Some(remaining) = skipped.get_mut(&key) {
                let step = (*remaining).min(buffer.len());
                buffer.drain(..step);
                *remaining -= step;
                if *remaining == 0 {
                    skipped.remove(&key);
                }
            }
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
                if total_headers + content_len > MAX_MESSAGE_BYTES {
                    // Too large to hold. Drop the headers and remember how much
                    // body to walk past, so the following messages still parse.
                    let already_here = buffer.len().saturating_sub(total_headers);
                    buffer.drain(..total_headers + already_here.min(content_len));
                    self.skipped_bodies
                        .lock()
                        .expect("skipped bodies poisoned")
                        .insert(key, content_len.saturating_sub(already_here));
                    continue;
                }
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
                let mut hosts = self.hosts.lock().expect("discovery hosts poisoned");
                // Connection ids grow monotonically, so the smallest one is the
                // oldest API connection and the right thing to forget.
                while hosts.len() >= MAX_TRACKED_API_CONNECTIONS
                    && let Some(oldest) = hosts.keys().min().copied()
                    && oldest != context.connection_id
                {
                    hosts.remove(&oldest);
                }
                hosts.insert(context.connection_id, host.clone());
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
            let path_is_player_stats = method.as_deref() == Some("POST")
                && path
                    .as_deref()
                    .is_some_and(|value| value.split('?').next() == Some(PLAYER_STATS_PATH));
            if status.is_none() && path_is_player_stats {
                messages.push(discovery_message(
                    context,
                    "stats_stream_ready",
                    &host,
                    method.as_deref(),
                    path.as_deref(),
                    status,
                    headers.get("content-type").map(String::as_str),
                    Value::Null,
                    None,
                ));
            }
            let decoded_body = decode_content(&body, headers.get("content-encoding"));
            let decoded_json = decoded_body
                .as_deref()
                .and_then(|body| serde_json::from_slice::<Value>(body).ok());
            let shape = decoded_json
                .as_ref()
                .map(|value| json_shape(value, 0))
                .unwrap_or(Value::Null);
            // Responses are paired with requests by order on the connection, and
            // one response we never saw shifts that pairing for good - the game
            // fires dozens of requests at once when the player returns to the
            // lobby. The body itself is the reliable marker, so it decides, and
            // a disagreement resynchronises the queue instead of silently
            // mislabelling every later response.
            let body_is_player_stats = status.is_some()
                && decoded_json
                    .as_ref()
                    .is_some_and(|value| value.get("scopedPlayerStats").is_some());
            if body_is_player_stats && !path_is_player_stats {
                method = Some("POST".to_owned());
                path = Some(PLAYER_STATS_PATH.to_owned());
                self.pending_requests
                    .lock()
                    .expect("pending requests poisoned")
                    .remove(&context.connection_id);
            }
            let is_player_stats = path_is_player_stats || body_is_player_stats;
            if status.is_some_and(|value| (200..300).contains(&value))
                && is_player_stats
                && let Some(stats) = decoded_json.as_ref()
            {
                let serialized = stats.to_string();
                messages.push(discovery_message(
                    context,
                    "player_stats_response",
                    &host,
                    method.as_deref(),
                    path.as_deref(),
                    status,
                    headers.get("content-type").map(String::as_str),
                    Value::Null,
                    Some(&serialized),
                ));
            }
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
            ));
        }

        // Only unframed data is capped. A message whose headers already arrived
        // is allowed to finish - trimming the front here used to cut the
        // statistics response in half and desynchronise everything after it.
        if buffer.len() > MAX_MESSAGE_BYTES {
            buffer.clear();
        } else if find(buffer, b"\r\n\r\n").is_none() && buffer.len() > MAX_SCAN_BUFFER {
            let drain = buffer.len() - MAX_SCAN_BUFFER;
            buffer.drain(..drain);
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
            FieldDescriptor::new("stats_body", DataKind::String).set_nullable(true),
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
    stats_body: Option<&str>,
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
    if let Some(value) = stats_body {
        put(&mut fields, "stats_body", value);
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

fn is_allowed_host(host: &str) -> bool {
    let Some(normalized) = normalize_host(host) else {
        return false;
    };
    let labels: Vec<_> = normalized.split('.').collect();
    labels.len() >= 4
        && matches!(
            labels.first().copied(),
            Some("api-gateway" | "client2pubsub" | "client2pubsub-ipv4")
        )
        && labels[labels.len() - 2..] == ["es-pio", "net"]
}

fn looks_like_regional_api_host(host: &str) -> bool {
    normalize_host(host).is_some_and(|host| {
        host.starts_with("api-gateway.")
            || host.ends_with(".es-pio.net")
            || host.contains("arc-raiders")
    })
}

fn normalize_host(host: &str) -> Option<String> {
    let normalized = host
        .trim()
        .trim_end_matches('.')
        .split(':')
        .next()?
        .to_ascii_lowercase();
    (!normalized.is_empty()
        && normalized
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-')))
    .then_some(normalized)
}

/// A TLS record carrying a ClientHello handshake message.
pub(crate) fn looks_like_client_hello(payload: &[u8]) -> bool {
    payload.len() > 5 && payload[0] == 0x16 && payload[5] == 0x01
}

/// A TLS record carrying a ServerHello handshake message.
pub(crate) fn looks_like_server_hello(payload: &[u8]) -> bool {
    payload.len() > 5 && payload[0] == 0x16 && payload[5] == 0x02
}

fn tls_client_hello_sni(payload: &[u8]) -> Option<String> {
    if payload.len() < 9 || payload[0] != 22 || payload[1] != 3 {
        return None;
    }
    let record_len = u16::from_be_bytes([payload[3], payload[4]]) as usize;
    let record = payload.get(5..5usize.checked_add(record_len)?)?;
    if record.len() < 4 || record[0] != 1 {
        return None;
    }
    let handshake_len =
        ((record[1] as usize) << 16) | ((record[2] as usize) << 8) | record[3] as usize;
    let hello = record.get(4..4usize.checked_add(handshake_len)?)?;
    let mut cursor = 2usize.checked_add(32)?;

    let session_len = *hello.get(cursor)? as usize;
    cursor = cursor.checked_add(1 + session_len)?;
    let cipher_len = read_u16(hello, cursor)? as usize;
    cursor = cursor.checked_add(2 + cipher_len)?;
    let compression_len = *hello.get(cursor)? as usize;
    cursor = cursor.checked_add(1 + compression_len)?;
    let extensions_len = read_u16(hello, cursor)? as usize;
    cursor = cursor.checked_add(2)?;
    let extensions_end = cursor.checked_add(extensions_len)?.min(hello.len());

    while cursor.checked_add(4)? <= extensions_end {
        let extension_type = read_u16(hello, cursor)?;
        let extension_len = read_u16(hello, cursor + 2)? as usize;
        cursor += 4;
        let extension_end = cursor.checked_add(extension_len)?;
        if extension_end > extensions_end {
            return None;
        }
        if extension_type == 0 {
            let list_len = read_u16(hello, cursor)? as usize;
            let mut name_cursor = cursor.checked_add(2)?;
            let list_end = name_cursor.checked_add(list_len)?.min(extension_end);
            while name_cursor.checked_add(3)? <= list_end {
                let name_type = *hello.get(name_cursor)?;
                let name_len = read_u16(hello, name_cursor + 1)? as usize;
                name_cursor += 3;
                let name_end = name_cursor.checked_add(name_len)?;
                if name_end > list_end {
                    return None;
                }
                if name_type == 0 {
                    return std::str::from_utf8(&hello[name_cursor..name_end])
                        .ok()
                        .and_then(normalize_host);
                }
                name_cursor = name_end;
            }
        }
        cursor = extension_end;
    }
    None
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_be_bytes([
        *bytes.get(offset)?,
        *bytes.get(offset.checked_add(1)?)?,
    ]))
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
        assert!(is_allowed_host("api-gateway.asia.es-pio.net"));
        assert!(is_allowed_host("client2pubsub.any-region.es-pio.net"));
        assert!(!is_allowed_host("api-gateway.europe.es-pio.net.evil.test"));
        assert!(!is_allowed_host("evil.es-pio.net"));
        assert!(looks_like_regional_api_host("api-gateway.unknown.example"));
    }

    #[test]
    fn discovers_regional_host_from_tls_client_hello() {
        let host = b"api-gateway.asia.es-pio.net";
        let mut server_name = Vec::new();
        server_name.extend_from_slice(&((host.len() + 3) as u16).to_be_bytes());
        server_name.push(0);
        server_name.extend_from_slice(&(host.len() as u16).to_be_bytes());
        server_name.extend_from_slice(host);

        let mut extension = vec![0, 0];
        extension.extend_from_slice(&(server_name.len() as u16).to_be_bytes());
        extension.extend_from_slice(&server_name);

        let mut hello = vec![3, 3];
        hello.extend_from_slice(&[0; 32]);
        hello.push(0);
        hello.extend_from_slice(&2u16.to_be_bytes());
        hello.extend_from_slice(&[0x13, 0x01]);
        hello.extend_from_slice(&[1, 0]);
        hello.extend_from_slice(&(extension.len() as u16).to_be_bytes());
        hello.extend_from_slice(&extension);

        let mut handshake = vec![1];
        handshake.extend_from_slice(&[
            ((hello.len() >> 16) & 0xff) as u8,
            ((hello.len() >> 8) & 0xff) as u8,
            (hello.len() & 0xff) as u8,
        ]);
        handshake.extend_from_slice(&hello);

        let mut record = vec![22, 3, 1];
        record.extend_from_slice(&(handshake.len() as u16).to_be_bytes());
        record.extend_from_slice(&handshake);

        assert_eq!(
            tls_client_hello_sni(&record).as_deref(),
            Some("api-gateway.asia.es-pio.net")
        );
    }

    #[test]
    fn emits_native_stats_once_from_the_games_regional_response() {
        let parser = DiscoveryParser::default();
        let request_context = StreamContext {
            connection_id: 7,
            direction: Direction::ToServer,
            src_ip: "127.0.0.1".parse().unwrap(),
            dst_ip: "127.0.0.2".parse().unwrap(),
            src_port: 50_000,
            dst_port: 443,
            bytes_parsed: 0,
            messages_parsed: 0,
            alpn: None,
        };
        let request = b"POST /v1/pioneer/stats/player-v2 HTTP/1.1\r\nHost: api-gateway.asia.es-pio.net\r\nAuthorization: Bearer header.payload.signature\r\nContent-Length: 2\r\n\r\n{}";
        let StreamParseResult::Complete {
            messages: request_messages,
            ..
        } = parser.parse_stream(request, &request_context)
        else {
            panic!("request was not parsed");
        };
        assert!(request_messages.iter().any(|message| {
            string_field(message, "kind").as_deref() == Some("stats_stream_ready")
        }));
        assert!(
            request_messages
                .iter()
                .all(|message| !message.fields.contains_key("token"))
        );

        let response_context = StreamContext {
            direction: Direction::ToClient,
            src_ip: request_context.dst_ip,
            dst_ip: request_context.src_ip,
            src_port: request_context.dst_port,
            dst_port: request_context.src_port,
            ..request_context
        };
        let body = br#"{"scopedPlayerStats":[]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            std::str::from_utf8(body).unwrap()
        );
        let StreamParseResult::Complete { messages, .. } =
            parser.parse_stream(response.as_bytes(), &response_context)
        else {
            panic!("response was not parsed");
        };
        let stats = messages
            .iter()
            .find(|message| {
                string_field(message, "kind").as_deref() == Some("player_stats_response")
            })
            .expect("native player stats response");
        assert_eq!(
            string_field(stats, "host").as_deref(),
            Some("api-gateway.asia.es-pio.net")
        );
        assert!(string_field(stats, "stats_body").is_some());
    }

    #[test]
    fn reads_a_statistics_response_larger_than_the_scan_buffer() {
        // The response grows with the account's history. Anything past the scan
        // buffer used to be trimmed away mid-message and never parsed at all.
        let parser = DiscoveryParser::default();
        let context = StreamContext {
            connection_id: 21,
            direction: Direction::ToClient,
            src_ip: "127.0.0.2".parse().unwrap(),
            dst_ip: "127.0.0.1".parse().unwrap(),
            src_port: 443,
            dst_port: 50_000,
            bytes_parsed: 0,
            messages_parsed: 0,
            alpn: None,
        };
        let rows: Vec<String> = (0..4_000)
            .map(|index| format!(r#"{{"eventId":{index},"targetId":-{index},"amount":{index}}}"#))
            .collect();
        let body = format!(
            r#"{{"scopedPlayerStats":[{{"playerStats":[{}]}}]}}"#,
            rows.join(",")
        );
        assert!(
            body.len() > MAX_SCAN_BUFFER,
            "the fixture must exceed the scan buffer, got {} bytes",
            body.len()
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nHost: api-gateway.europe.es-pio.net\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );

        // Arrives in pieces, the way a large body actually does.
        let bytes = response.as_bytes();
        let mut found = false;
        for chunk in bytes.chunks(16 * 1024) {
            if let StreamParseResult::Complete { messages, .. } =
                parser.parse_stream(chunk, &context)
                && messages.iter().any(|message| {
                    string_field(message, "kind").as_deref() == Some("player_stats_response")
                })
            {
                found = true;
            }
        }
        assert!(
            found,
            "the oversized statistics response must still be read"
        );
    }

    #[test]
    fn stays_framed_when_a_body_is_too_large_to_assemble() {
        let parser = DiscoveryParser::default();
        let context = StreamContext {
            connection_id: 22,
            direction: Direction::ToClient,
            src_ip: "127.0.0.2".parse().unwrap(),
            dst_ip: "127.0.0.1".parse().unwrap(),
            src_port: 443,
            dst_port: 50_000,
            bytes_parsed: 0,
            messages_parsed: 0,
            alpn: None,
        };
        let huge = MAX_MESSAGE_BYTES + 1;
        parser.parse_stream(
            format!(
                "HTTP/1.1 200 OK\r\nHost: api-gateway.europe.es-pio.net\r\nContent-Length: {huge}\r\n\r\n"
            )
            .as_bytes(),
            &context,
        );
        // The body never arrives in full; feed a slice of it and then a normal
        // statistics response behind it.
        let filler = vec![b'x'; 64 * 1024];
        for _ in 0..4 {
            parser.parse_stream(&filler, &context);
        }
        let remaining = huge - 4 * filler.len();
        parser.parse_stream(&vec![b'x'; remaining], &context);

        let body = r#"{"scopedPlayerStats":[]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nHost: api-gateway.europe.es-pio.net\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let StreamParseResult::Complete { messages, .. } =
            parser.parse_stream(response.as_bytes(), &context)
        else {
            panic!("response was not parsed");
        };
        assert!(
            messages.iter().any(|message| {
                string_field(message, "kind").as_deref() == Some("player_stats_response")
            }),
            "the message after an oversized body must still be framed correctly"
        );
    }

    #[test]
    fn reads_the_statistics_even_after_the_request_pairing_drifted() {
        // The lobby burst loses one response, so every later response on this
        // connection would be paired with the wrong request. The statistics must
        // still be read, and the pairing must recover.
        let parser = DiscoveryParser::default();
        let to_server = StreamContext {
            connection_id: 11,
            direction: Direction::ToServer,
            src_ip: "127.0.0.1".parse().unwrap(),
            dst_ip: "127.0.0.2".parse().unwrap(),
            src_port: 50_000,
            dst_port: 443,
            bytes_parsed: 0,
            messages_parsed: 0,
            alpn: None,
        };
        let connection_id = to_server.connection_id;
        let to_client = StreamContext {
            connection_id,
            direction: Direction::ToClient,
            src_ip: to_server.dst_ip,
            dst_ip: to_server.src_ip,
            src_port: to_server.dst_port,
            dst_port: to_server.src_port,
            bytes_parsed: 0,
            messages_parsed: 0,
            alpn: None,
        };

        for path in ["/v1/shared/heartbeat", "/v1/pioneer/inventory"] {
            parser.parse_stream(
                format!(
                    "POST {path} HTTP/1.1\r\nHost: api-gateway.europe.es-pio.net\r\nContent-Length: 0\r\n\r\n"
                )
                .as_bytes(),
                &to_server,
            );
        }
        parser.parse_stream(
            b"POST /v1/pioneer/stats/player-v2 HTTP/1.1\r\nHost: api-gateway.europe.es-pio.net\r\nContent-Length: 0\r\n\r\n",
            &to_server,
        );

        // Only the statistics answer comes back; the two before it were missed.
        let body = br#"{"scopedPlayerStats":[]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            std::str::from_utf8(body).unwrap()
        );
        let StreamParseResult::Complete { messages, .. } =
            parser.parse_stream(response.as_bytes(), &to_client)
        else {
            panic!("response was not parsed");
        };
        let stats = messages
            .iter()
            .find(|message| {
                string_field(message, "kind").as_deref() == Some("player_stats_response")
            })
            .expect("the statistics response must be recognised by its body");
        assert_eq!(
            string_field(stats, "path").as_deref(),
            Some(PLAYER_STATS_PATH)
        );
        assert!(
            parser
                .pending_requests
                .lock()
                .expect("pending requests poisoned")
                .get(&connection_id)
                .is_none_or(|queue| queue.is_empty()),
            "the drifted queue must be dropped so later responses pair correctly"
        );
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

    #[test]
    fn keeps_the_game_api_connection_while_other_traffic_floods_in() {
        fn context(connection_id: u64) -> StreamContext {
            StreamContext {
                connection_id,
                direction: Direction::ToServer,
                src_ip: "127.0.0.1".parse().unwrap(),
                dst_ip: "127.0.0.2".parse().unwrap(),
                src_port: 50_000,
                dst_port: 443,
                bytes_parsed: 0,
                messages_parsed: 0,
                alpn: None,
            }
        }

        // The game asks for its statistics once, then goes quiet for a raid
        // while the rest of the machine keeps opening connections.
        let parser = DiscoveryParser::default();
        let game = 1u64;
        parser.parse_stream(
            b"POST /v1/pioneer/stats/player-v2 HTTP/1.1\r\nHost: api-gateway.europe.es-pio.net\r\nContent-Length: 0\r\n\r\n",
            &context(game),
        );
        assert!(
            parser
                .hosts
                .lock()
                .expect("discovery hosts poisoned")
                .contains_key(&game)
        );

        for connection_id in 2..=(MAX_DISCOVERY_CONNECTIONS as u64 + 500) {
            parser.parse_stream(b"incomplete", &context(connection_id));
        }

        assert!(
            parser
                .last_seen
                .lock()
                .expect("last seen map poisoned")
                .contains_key(&game),
            "the connection carrying the statistics must survive the flood"
        );
        assert!(
            parser
                .hosts
                .lock()
                .expect("discovery hosts poisoned")
                .contains_key(&game)
        );
    }

    #[test]
    fn bounds_the_number_of_tracked_api_connections() {
        let parser = DiscoveryParser::default();
        for connection_id in 1..=(MAX_TRACKED_API_CONNECTIONS as u64 * 2) {
            parser.parse_stream(
                b"POST /v1/pioneer/stats/player-v2 HTTP/1.1\r\nHost: api-gateway.europe.es-pio.net\r\nContent-Length: 0\r\n\r\n",
                &StreamContext {
                    connection_id,
                    direction: Direction::ToServer,
                    src_ip: "127.0.0.1".parse().unwrap(),
                    dst_ip: "127.0.0.2".parse().unwrap(),
                    src_port: 50_000,
                    dst_port: 443,
                    bytes_parsed: 0,
                    messages_parsed: 0,
                    alpn: None,
                },
            );
        }
        let hosts = parser.hosts.lock().expect("discovery hosts poisoned");
        assert!(hosts.len() <= MAX_TRACKED_API_CONNECTIONS);
        assert!(
            hosts.contains_key(&(MAX_TRACKED_API_CONNECTIONS as u64 * 2)),
            "the newest API connection must always be tracked"
        );
    }
}
