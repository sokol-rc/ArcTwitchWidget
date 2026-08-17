use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use arc_live_collector::{
    CollectorEvent, CollectorHandle, DEFAULT_SERVICE_ADDRESS, SERVICE_PROTOCOL_VERSION,
    ServiceRequest, parse_local_service_address, service_address_file,
};
use crossbeam_channel::{Receiver, bounded};

pub enum CollectorRuntime {
    Local(CollectorHandle),
    Remote(RemoteCollector),
}

impl CollectorRuntime {
    pub fn connect_or_start_local(keylog_path: PathBuf, auth_token: String) -> Self {
        match RemoteCollector::connect(keylog_path.clone(), auth_token) {
            Ok(remote) => Self::Remote(remote),
            Err(_) => Self::Local(arc_live_collector::start_local(keylog_path, false)),
        }
    }

    pub fn events(&self) -> &Receiver<CollectorEvent> {
        match self {
            Self::Local(handle) => &handle.events,
            Self::Remote(handle) => &handle.events,
        }
    }

    pub fn stop(&self) {
        match self {
            Self::Local(handle) => handle.stop(),
            Self::Remote(handle) => handle.stop(),
        }
    }
}

pub struct RemoteCollector {
    pub events: Receiver<CollectorEvent>,
    stop: Arc<AtomicBool>,
    socket: TcpStream,
    worker: Option<thread::JoinHandle<()>>,
}

/// How long to wait before reaching for the service again after a lost
/// connection. The service is restarted by every update, so a session that
/// cannot come back on its own would leave the widget frozen for the stream.
const RECONNECT_DELAY: Duration = Duration::from_secs(3);
/// A single event never comes close to this; anything larger means the stream
/// is out of step, and starting a fresh line beats growing without end.
const MAX_EVENT_BYTES: usize = 32 * 1024 * 1024;

impl RemoteCollector {
    fn open(keylog_path: PathBuf, auth_token: String) -> Result<(BufReader<TcpStream>, TcpStream)> {
        let mut addresses = Vec::new();
        if let Some(path) = service_address_file()
            && let Ok(value) = std::fs::read_to_string(path)
            && let Some(address) = parse_local_service_address(&value)
        {
            addresses.push(address);
        }
        let default_address: SocketAddr = DEFAULT_SERVICE_ADDRESS.parse()?;
        if !addresses.contains(&default_address) {
            addresses.push(default_address);
        }
        let mut last_error = None;
        let mut socket = None;
        for address in addresses {
            match TcpStream::connect_timeout(&address, Duration::from_millis(500)) {
                Ok(connected) => {
                    socket = Some(connected);
                    break;
                }
                Err(error) => last_error = Some(error),
            }
        }
        let mut socket = socket.ok_or_else(|| {
            anyhow::anyhow!(
                "connecting to ARC Live Capture Service failed: {}",
                last_error
                    .map(|error| error.to_string())
                    .unwrap_or_else(|| "no service address".to_owned())
            )
        })?;
        socket.set_nodelay(true)?;
        let request = ServiceRequest {
            protocol_version: SERVICE_PROTOCOL_VERSION,
            keylog_path,
            auth_token,
        };
        serde_json::to_writer(&mut socket, &request)?;
        socket.write_all(b"\n")?;
        socket.flush()?;

        let reader_socket = socket.try_clone()?;
        reader_socket.set_read_timeout(Some(Duration::from_secs(2)))?;
        let mut reader = BufReader::new(reader_socket);
        let mut handshake_line = String::new();
        ensure!(
            reader.read_line(&mut handshake_line)? > 0,
            "capture service closed before handshake"
        );
        let handshake: CollectorEvent = serde_json::from_str(&handshake_line)?;
        ensure!(
            matches!(
                &handshake,
                CollectorEvent::Connected { version, .. } if version == env!("CARGO_PKG_VERSION")
            ),
            "capture service version does not match the application"
        );
        reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(500)))?;
        Ok((reader, socket))
    }

    fn connect(keylog_path: PathBuf, auth_token: String) -> Result<Self> {
        let (mut reader, socket) = Self::open(keylog_path.clone(), auth_token.clone())?;
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let (tx, events) = bounded(256);
        let worker = thread::spawn(move || {
            // Bytes, not a String, and kept across iterations: the read timeout
            // can land in the middle of an event - even in the middle of a
            // multi-byte character - and anything already read would otherwise
            // be thrown away, taking the event and the framing with it.
            let mut line = Vec::new();
            'session: while !worker_stop.load(Ordering::Relaxed) {
                while !worker_stop.load(Ordering::Relaxed) {
                    match reader.read_until(b'\n', &mut line) {
                        Ok(0) => break,
                        Ok(_) if !line.ends_with(b"\n") => continue,
                        Ok(_) => {
                            match std::str::from_utf8(&line)
                                .map_err(|error| error.to_string())
                                .and_then(|text| {
                                    serde_json::from_str::<CollectorEvent>(text)
                                        .map_err(|error| error.to_string())
                                }) {
                                Ok(event) => {
                                    let _ = tx.try_send(event);
                                }
                                Err(error) => {
                                    let _ = tx.try_send(CollectorEvent::Error(format!(
                                        "Capture service returned invalid data: {error}"
                                    )));
                                }
                            }
                            line.clear();
                        }
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) => {}
                        Err(error) => {
                            let _ = tx.try_send(CollectorEvent::Error(format!(
                                "Capture service connection failed: {error}"
                            )));
                            break;
                        }
                    }
                    if line.len() > MAX_EVENT_BYTES {
                        let _ = tx.try_send(CollectorEvent::Error(
                            "Capture service sent an oversized event; resynchronising".to_owned(),
                        ));
                        line.clear();
                    }
                }
                if worker_stop.load(Ordering::Relaxed) {
                    break;
                }
                // The connection is gone. Every update restarts the service, so
                // the only useful answer is to keep reaching for it rather than
                // leaving the widget frozen for the rest of the stream.
                let _ = tx.try_send(CollectorEvent::Status(
                    "Capture service connection lost, reconnecting".to_owned(),
                ));
                loop {
                    let deadline = Instant::now() + RECONNECT_DELAY;
                    while Instant::now() < deadline {
                        if worker_stop.load(Ordering::Relaxed) {
                            break 'session;
                        }
                        thread::sleep(Duration::from_millis(100));
                    }
                    match Self::open(keylog_path.clone(), auth_token.clone()) {
                        Ok((fresh, _)) => {
                            reader = fresh;
                            line.clear();
                            let _ = tx.try_send(CollectorEvent::Status(
                                "Capture service connection restored".to_owned(),
                            ));
                            continue 'session;
                        }
                        Err(error) => {
                            let _ = tx.try_send(CollectorEvent::Error(format!(
                                "Reconnecting to the capture service failed: {error:#}"
                            )));
                        }
                    }
                }
            }
            let _ = tx.try_send(CollectorEvent::Stopped);
        });
        Ok(Self {
            events,
            stop,
            socket,
            worker: Some(worker),
        })
    }

    fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}

impl Drop for RemoteCollector {
    fn drop(&mut self) {
        self.stop();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
