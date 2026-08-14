use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use arc_live_collector::{
    DEFAULT_SERVICE_ADDRESS, SERVICE_PROTOCOL_VERSION, ServiceRequest, service_address_file,
    start_local,
};
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

#[cfg(windows)]
use windows_service::{
    define_windows_service,
    service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
};

const SERVICE_NAME: &str = "ArcLiveCapture";

fn main() {
    let _log_guard = initialize_logging();
    let result = if std::env::args().any(|arg| arg == "--console") {
        run_server(Arc::new(AtomicBool::new(false)))
    } else {
        run_as_service()
    };
    if let Err(error) = result {
        eprintln!("ARC Live Capture Service failed: {error:#}");
        std::process::exit(1);
    }
}

fn initialize_logging() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let program_data = std::env::var_os("PROGRAMDATA")?;
    let logs = std::path::PathBuf::from(program_data)
        .join("ARC Live")
        .join("logs");
    std::fs::create_dir_all(&logs).ok()?;
    cleanup_old_logs(&logs, 7);
    let file = tracing_appender::rolling::daily(logs, "capture-service.log");
    let (writer, guard) = tracing_appender::non_blocking(file);
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(writer)
        .try_init()
        .ok()?;
    Some(guard)
}

fn cleanup_old_logs(directory: &Path, keep_days: u64) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let cutoff =
        std::time::SystemTime::now().checked_sub(Duration::from_secs(keep_days * 24 * 60 * 60));
    for entry in entries.flatten() {
        let path = entry.path();
        let matches = path
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|name| name.starts_with("capture-service.log"));
        let old = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .zip(cutoff)
            .is_some_and(|(modified, cutoff)| modified < cutoff);
        if matches && old {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(windows)]
define_windows_service!(ffi_service_main, service_main);

#[cfg(windows)]
fn run_as_service() -> Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .context("registering with Windows Service Control Manager")
}

#[cfg(not(windows))]
fn run_as_service() -> Result<()> {
    anyhow::bail!("ARC Live Capture Service is supported on Windows only")
}

#[cfg(windows)]
fn service_main(_arguments: Vec<std::ffi::OsString>) {
    if let Err(error) = service_main_inner() {
        tracing::error!(%error, "capture service stopped");
        eprintln!("ARC Live Capture Service stopped: {error:#}");
    }
}

#[cfg(windows)]
fn service_main_inner() -> Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    let handler_stop = Arc::clone(&stop);
    let status_handle =
        service_control_handler::register(SERVICE_NAME, move |control| match control {
            ServiceControl::Stop => {
                handler_stop.store(true, Ordering::Relaxed);
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        })
        .context("registering service control handler")?;
    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::ZERO,
        process_id: None,
    })?;
    let result = run_server(Arc::clone(&stop));
    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: if result.is_ok() {
            ServiceExitCode::Win32(0)
        } else {
            ServiceExitCode::Win32(1)
        },
        checkpoint: 0,
        wait_hint: Duration::ZERO,
        process_id: None,
    })?;
    result
}

fn run_server(stop: Arc<AtomicBool>) -> Result<()> {
    let (listener, address) = bind_service_listener()?;
    listener.set_nonblocking(true)?;
    publish_service_address(&address.to_string())?;
    tracing::info!(address = %address, "capture service ready");

    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Err(error) = serve_client(stream, Arc::clone(&stop)) {
                    tracing::warn!(%error, "capture client disconnected");
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn bind_service_listener() -> Result<(TcpListener, std::net::SocketAddr)> {
    bind_service_listener_at(DEFAULT_SERVICE_ADDRESS)
}

fn bind_service_listener_at(preferred: &str) -> Result<(TcpListener, std::net::SocketAddr)> {
    let listener = TcpListener::bind(preferred)
        .or_else(|_| TcpListener::bind("127.0.0.1:0"))
        .context("binding capture service to a local address")?;
    let address = listener.local_addr()?;
    Ok((listener, address))
}

fn publish_service_address(address: &str) -> Result<()> {
    let Some(path) = service_address_file() else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, address)
        .with_context(|| format!("publishing capture service address to {}", path.display()))
}

fn serve_client(mut stream: TcpStream, service_stop: Arc<AtomicBool>) -> Result<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let reader_stream = stream.try_clone()?;
    let mut reader = BufReader::new(reader_stream);
    let mut request_line = String::new();
    reader
        .by_ref()
        .take(16 * 1024)
        .read_line(&mut request_line)
        .context("reading service request")?;
    ensure!(!request_line.is_empty(), "empty service request");
    let mut request: ServiceRequest =
        serde_json::from_str(&request_line).context("parsing service request")?;
    ensure!(
        request.protocol_version == SERVICE_PROTOCOL_VERSION,
        "unsupported service protocol {}",
        request.protocol_version
    );
    validate_keylog_path(&request.keylog_path)?;
    authenticate_request(&request)?;
    request.auth_token.zeroize();
    let collector = start_local(request.keylog_path, true);
    while !service_stop.load(Ordering::Relaxed) {
        match collector.events.recv_timeout(Duration::from_millis(250)) {
            Ok(event) => {
                serde_json::to_writer(&mut stream, &event)?;
                stream.write_all(b"\n")?;
                stream.flush()?;
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }
    collector.stop();
    Ok(())
}

fn validate_keylog_path(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "keylog path must be absolute");
    ensure!(
        path.file_name().and_then(|name| name.to_str()) == Some("arc-live-tls.keys"),
        "unexpected keylog file name"
    );
    let text = path.to_string_lossy().to_ascii_lowercase();
    ensure!(
        text.contains("\\arclive\\arc live\\") || text.contains("\\arc live\\"),
        "keylog path is outside ARC Live data"
    );
    Ok(())
}

fn authenticate_request(request: &ServiceRequest) -> Result<()> {
    ensure!(
        request.auth_token.len() == 64,
        "invalid service authentication"
    );
    let root = request
        .keylog_path
        .parent()
        .and_then(Path::parent)
        .context("keylog path has no ARC Live data root")?;
    let expected = std::fs::read_to_string(root.join("service-token"))
        .context("reading local service authentication token")?;
    let supplied_hash = Sha256::digest(request.auth_token.as_bytes());
    let expected_hash = Sha256::digest(expected.trim().as_bytes());
    ensure!(
        supplied_hash == expected_hash,
        "service authentication failed"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_accepts_arc_live_keylog_paths() {
        assert!(
            validate_keylog_path(Path::new(
                r"C:\Users\demo\AppData\Local\ArcLive\ARC Live\data\sessions\arc-live-tls.keys"
            ))
            .is_ok()
        );
        assert!(validate_keylog_path(Path::new(r"C:\Windows\secret.keys")).is_err());
    }

    #[test]
    fn local_token_authenticates_capture_client() {
        let base = std::env::temp_dir().join(format!(
            "arc-live-service-test-{:016x}",
            rand::random::<u64>()
        ));
        let root = base.join("ArcLive").join("ARC Live").join("data");
        let sessions = root.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(root.join("service-token"), "a".repeat(64)).unwrap();
        let request = ServiceRequest {
            protocol_version: SERVICE_PROTOCOL_VERSION,
            keylog_path: sessions.join("arc-live-tls.keys"),
            auth_token: "a".repeat(64),
        };
        assert!(authenticate_request(&request).is_ok());
        let mut wrong = request;
        wrong.auth_token = "b".repeat(64);
        assert!(authenticate_request(&wrong).is_err());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn capture_service_falls_back_when_preferred_port_is_busy() {
        let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
        let preferred = occupied.local_addr().unwrap();
        let (listener, selected) = bind_service_listener_at(&preferred.to_string()).unwrap();
        assert_ne!(selected, preferred);
        assert!(selected.ip().is_loopback());
        drop(listener);
    }
}
