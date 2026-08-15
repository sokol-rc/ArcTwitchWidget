use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use arc_live_capture::{CaptureEvent, CaptureStats};
use arc_live_core::redaction::json_shape;
use arc_live_core::state::OverlayStats;
use arc_live_core::stats::normalize_player_stats;
use chrono::{DateTime, Utc};
use crossbeam_channel::{Receiver, Sender, bounded};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SERVICE_PROTOCOL_VERSION: u8 = 3;
pub const DEFAULT_SERVICE_ADDRESS: &str = "127.0.0.1:17843";

pub fn service_address_file() -> Option<PathBuf> {
    std::env::var_os("PROGRAMDATA").map(|root| {
        PathBuf::from(root)
            .join("ARC Live")
            .join("capture-service.address")
    })
}

pub fn parse_local_service_address(value: &str) -> Option<SocketAddr> {
    let address: SocketAddr = value.trim().parse().ok()?;
    address.ip().is_loopback().then_some(address)
}

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
        stats_stream_ready: bool,
    },
    Probe(Box<ProbePayload>),
    Error(String),
    Stopped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbePayload {
    pub observed_at: DateTime<Utc>,
    pub host: String,
    pub status: u16,
    pub content_type: Option<String>,
    pub shape: Value,
    pub overlay: OverlayStats,
    pub unknown_event_rows: u64,
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
    let capture = arc_live_capture::start_capture(keylog_path);
    let mut stats_stream_ready = false;

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
            Ok(CaptureEvent::StatsStreamReady { host }) => {
                stats_stream_ready = true;
                tx.try_send(CollectorEvent::Status(format!(
                    "Regional player statistics stream observed ({host})"
                )))
                .ok();
                send_ready(tx, true);
            }
            Ok(CaptureEvent::PlayerStatsResponse {
                host,
                status,
                content_type,
                body,
            }) => {
                if !stats_stream_ready {
                    stats_stream_ready = true;
                    send_ready(tx, true);
                }
                match native_stats_payload(host, status, content_type, body) {
                    Ok(payload) => {
                        tx.try_send(CollectorEvent::Probe(Box::new(payload))).ok();
                    }
                    Err(error) => {
                        tx.try_send(CollectorEvent::Error(format!(
                            "Reading game player stats response failed: {error:#}"
                        )))
                        .ok();
                    }
                }
            }
            Ok(CaptureEvent::Error(message)) => {
                tx.try_send(CollectorEvent::Error(message)).ok();
            }
            Ok(CaptureEvent::Stopped) => break,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }
    capture.stop();
    Ok(())
}

fn send_ready(tx: &Sender<CollectorEvent>, stats_stream_ready: bool) {
    tx.try_send(CollectorEvent::Ready { stats_stream_ready })
        .ok();
}

fn native_stats_payload(
    host: String,
    status: u16,
    content_type: Option<String>,
    value: Value,
) -> Result<ProbePayload> {
    let (overlay, unknown_event_rows) = normalize_player_stats(&value)?;
    Ok(ProbePayload {
        observed_at: Utc::now(),
        host,
        status,
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

    #[test]
    fn service_discovery_only_accepts_loopback_addresses() {
        assert!(parse_local_service_address("127.0.0.1:43123").is_some());
        assert!(parse_local_service_address("[::1]:43123").is_some());
        assert!(parse_local_service_address("0.0.0.0:43123").is_none());
        assert!(parse_local_service_address("example.test:43123").is_none());
    }
}
