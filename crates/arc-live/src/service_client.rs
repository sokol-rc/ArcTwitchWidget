use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use arc_live_collector::{
    CollectorEvent, CollectorHandle, DEFAULT_SERVICE_ADDRESS, SERVICE_PROTOCOL_VERSION,
    ServiceRequest,
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
        let address: SocketAddr = DEFAULT_SERVICE_ADDRESS.parse()?;
        let mut socket = TcpStream::connect_timeout(&address, Duration::from_millis(500))
            .context("connecting to ARC Live Capture Service")?;
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
        reader_socket.set_read_timeout(Some(Duration::from_millis(500)))?;
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let (tx, events) = bounded(256);
        let worker = thread::spawn(move || {
            let mut reader = BufReader::new(reader_socket);
            while !worker_stop.load(Ordering::Relaxed) {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => match serde_json::from_str::<CollectorEvent>(&line) {
                        Ok(event) => {
                            let _ = tx.try_send(event);
                        }
                        Err(error) => {
                            let _ = tx.try_send(CollectorEvent::Error(format!(
                                "Capture service returned invalid data: {error}"
                            )));
                        }
                    },
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
