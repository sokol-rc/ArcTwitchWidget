use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

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

impl RemoteCollector {
    fn connect(keylog_path: PathBuf, auth_token: String) -> Result<Self> {
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
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let (tx, events) = bounded(256);
        let _ = tx.try_send(handshake);
        let worker = thread::spawn(move || {
            // The line outlives the iteration on purpose: the read timeout can
            // land in the middle of a long event, and dropping what already
            // arrived would lose that event and desynchronise the next one.
            let mut line = String::new();
            while !worker_stop.load(Ordering::Relaxed) {
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) if !line.ends_with('\n') => continue,
                    Ok(_) => {
                        match serde_json::from_str::<CollectorEvent>(&line) {
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
