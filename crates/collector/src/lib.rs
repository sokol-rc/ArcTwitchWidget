use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use arc_live_capture::{CaptureEvent, CaptureStats};
use arc_live_core::redaction::json_shape;
use arc_live_core::state::OverlayStats;
use arc_live_core::stats::normalize_player_stats;
use chrono::{DateTime, Utc};
use crossbeam_channel::{Receiver, Sender, bounded};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use zeroize::{Zeroize, Zeroizing};

const ENDPOINT: &str = "https://api-gateway.europe.es-pio.net/v1/pioneer/stats/player-v2";
const MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;
const LIVE_SYNC_INTERVAL: Duration = Duration::from_secs(15);
const LIVE_SYNC_ERROR_BACKOFF: Duration = Duration::from_secs(30);

pub const SERVICE_PROTOCOL_VERSION: u8 = 1;
pub const DEFAULT_SERVICE_ADDRESS: &str = "127.0.0.1:17843";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceRequest {
    pub protocol_version: u8,
    pub keylog_path: PathBuf,
    pub auth_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum CollectorEvent {
    Connected {
        version: String,
        privileged_service: bool,
    },
    Status(String),
    Stats(Box<CaptureStats>),
    Observation(Value),
    Ready {
        token_seen: bool,
        request_template_seen: bool,
    },
    Probe(Box<ProbePayload>),
    Error(String),
    Stopped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbePayload {
    pub observed_at: DateTime<Utc>,
    pub status: u16,
    pub content_type: Option<String>,
    pub shape: Value,
    pub overlay: OverlayStats,
    pub unknown_event_rows: u64,
}

#[derive(Clone)]
struct SensitiveRequestTemplate {
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Drop for SensitiveRequestTemplate {
    fn drop(&mut self) {
        for (name, value) in &mut self.headers {
            name.zeroize();
            value.zeroize();
        }
        self.body.zeroize();
    }
}

pub struct CollectorHandle {
    pub events: Receiver<CollectorEvent>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl CollectorHandle {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for CollectorHandle {
    fn drop(&mut self) {
        self.stop();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub fn start_local(keylog_path: PathBuf, privileged_service: bool) -> CollectorHandle {
    let (tx, events) = bounded(256);
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    let worker = thread::spawn(move || {
        let _ = tx.try_send(CollectorEvent::Connected {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            privileged_service,
        });
        if let Err(error) = run_collector(keylog_path, worker_stop, &tx) {
            let _ = tx.try_send(CollectorEvent::Error(format!("{error:#}")));
        }
        let _ = tx.try_send(CollectorEvent::Stopped);
    });
    CollectorHandle {
        events,
        stop,
        worker: Some(worker),
    }
}

fn run_collector(
    keylog_path: PathBuf,
    stop: Arc<AtomicBool>,
    tx: &Sender<CollectorEvent>,
) -> Result<()> {
    let probe_client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .pool_max_idle_per_host(1)
        .build()
        .context("building stats client")?;
    let capture = arc_live_capture::start_capture(keylog_path);
    let (probe_tx, probe_rx) = bounded::<Result<ProbePayload, String>>(1);
    let mut token: Option<Zeroizing<String>> = None;
    let mut template: Option<SensitiveRequestTemplate> = None;
    let mut probe_in_flight = false;
    let mut next_probe = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        match capture.events.recv_timeout(Duration::from_millis(200)) {
            Ok(CaptureEvent::Status(message)) => {
                tx.try_send(CollectorEvent::Status(message)).ok();
            }
            Ok(CaptureEvent::Stats(stats)) => {
                tx.try_send(CollectorEvent::Stats(stats)).ok();
            }
            Ok(CaptureEvent::Observation(value)) => {
                tx.try_send(CollectorEvent::Observation(value)).ok();
            }
            Ok(CaptureEvent::Token {
                token: next_token,
                fingerprint,
            }) => {
                token = Some(Zeroizing::new(next_token));
                tx.try_send(CollectorEvent::Status(format!(
                    "Embark credentials observed ({fingerprint})"
                )))
                .ok();
                send_ready(tx, token.is_some(), template.is_some());
                next_probe = Instant::now();
            }
            Ok(CaptureEvent::StatsRequestTemplate { headers, body }) => {
                template = Some(SensitiveRequestTemplate { headers, body });
                tx.try_send(CollectorEvent::Status(
                    "Player statistics request context observed".to_owned(),
                ))
                .ok();
                send_ready(tx, token.is_some(), template.is_some());
                next_probe = Instant::now();
            }
            Ok(CaptureEvent::Error(message)) => {
                tx.try_send(CollectorEvent::Error(message)).ok();
            }
            Ok(CaptureEvent::Stopped) => break,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }

        while let Ok(result) = probe_rx.try_recv() {
            probe_in_flight = false;
            match result {
                Ok(payload) => {
                    tx.try_send(CollectorEvent::Probe(Box::new(payload))).ok();
                    next_probe = Instant::now() + LIVE_SYNC_INTERVAL;
                }
                Err(error) => {
                    tx.try_send(CollectorEvent::Error(error)).ok();
                    next_probe = Instant::now() + LIVE_SYNC_ERROR_BACKOFF;
                }
            }
        }

        if !probe_in_flight
            && Instant::now() >= next_probe
            && let (Some(token), Some(template)) = (&token, &template)
        {
            let token = token.to_string();
            let template = template.clone();
            let probe_tx = probe_tx.clone();
            let probe_client = probe_client.clone();
            probe_in_flight = true;
            thread::spawn(move || {
                let token = Zeroizing::new(token);
                let result = probe(&probe_client, &token, &template.headers, &template.body)
                    .map_err(|error| format!("Player stats sync failed: {error:#}"));
                let _ = probe_tx.try_send(result);
            });
        }
    }
    capture.stop();
    Ok(())
}

fn send_ready(tx: &Sender<CollectorEvent>, token_seen: bool, request_template_seen: bool) {
    tx.try_send(CollectorEvent::Ready {
        token_seen,
        request_template_seen,
    })
    .ok();
}

fn probe(
    client: &reqwest::blocking::Client,
    token: &str,
    headers: &[(String, String)],
    request_body: &[u8],
) -> Result<ProbePayload> {
    let mut request = client.post(ENDPOINT);
    for (name, value) in headers {
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .with_context(|| format!("invalid captured request header name: {name}"))?;
        let value = reqwest::header::HeaderValue::from_str(value)
            .context("invalid captured request header value")?;
        request = request.header(name, value);
    }
    let mut response = request
        .bearer_auth(token)
        .body(request_body.to_vec())
        .send()
        .context("sending read-only player stats request")?;
    let status = response.status();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    if !status.is_success() {
        bail!("player stats endpoint returned HTTP {}", status.as_u16());
    }
    let mut body = Vec::new();
    response
        .by_ref()
        .take(MAX_RESPONSE_BYTES + 1)
        .read_to_end(&mut body)
        .context("reading player stats response")?;
    if body.len() > MAX_RESPONSE_BYTES as usize {
        bail!("player stats response exceeded 4 MiB safety limit");
    }
    let value: Value =
        serde_json::from_slice(&body).context("player stats response was not JSON")?;
    let (overlay, unknown_event_rows) = normalize_player_stats(&value)?;
    Ok(ProbePayload {
        observed_at: Utc::now(),
        status: status.as_u16(),
        content_type,
        shape: json_shape(&value, 0),
        overlay,
        unknown_event_rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_request_round_trips_without_credentials() {
        let request = ServiceRequest {
            protocol_version: SERVICE_PROTOCOL_VERSION,
            keylog_path: PathBuf::from(
                r"C:\Users\demo\AppData\Local\ArcLive\ARC Live\data\sessions\arc-live-tls.keys",
            ),
            auth_token: "a".repeat(64),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(!json.contains("bearer"));
        let restored: ServiceRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.protocol_version, SERVICE_PROTOCOL_VERSION);
    }
}
