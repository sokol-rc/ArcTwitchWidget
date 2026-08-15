//! Packet source built on Windows raw sockets (`SIO_RCVALL`) instead of a
//! kernel driver. It delivers bare IPv4 packets, exactly what `packet.rs`
//! already parses, so the TLS and stream layers stay untouched.
//!
//! The process still has to run elevated - raw sockets require Administrator -
//! which the LocalSystem capture service already satisfies. Nothing is ever
//! sent: the sockets are receive-only, and no driver is installed or loaded.
//!
//! The technique (notably `RCVALL_IPLEVEL` rather than full promiscuous mode)
//! is the one used by ARCTracker Sync; this is an independent implementation,
//! written from the documented Winsock API, not a copy of their source.

#[cfg(windows)]
mod platform {
    use std::net::Ipv4Addr;
    use std::ptr::{null, null_mut};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};

    use anyhow::{Result, bail};
    use windows_sys::Win32::Networking::WinSock::{
        ADDRESS_FAMILY, ADDRINFOA, AF_INET, FIONBIO, IN_ADDR, IN_ADDR_0, INVALID_SOCKET,
        IPPROTO_IP, SO_RCVBUF, SOCK_RAW, SOCKADDR, SOCKADDR_IN, SOCKET, SOCKET_ERROR, SOL_SOCKET,
        WSADATA, WSAEMSGSIZE, WSAEWOULDBLOCK, WSAGetLastError, WSAIoctl, WSAStartup, bind,
        closesocket, freeaddrinfo, getaddrinfo, gethostname, ioctlsocket, recv, setsockopt, socket,
    };

    /// `_WSAIOW(IOC_VENDOR, 1)`.
    const SIO_RCVALL: u32 = 0x9800_0001;
    /// Everything to and from this interface's own IP, both directions, without
    /// promiscuous mode. Full promiscuous (`RCVALL_ON`) is unreliable for the
    /// host's own inbound traffic on Wi-Fi and loses ServerHello, which breaks
    /// TLS key establishment.
    const RCVALL_IPLEVEL: u32 = 3;
    /// Large enough for LSO/GRO coalesced segments; anything bigger is dropped
    /// by Winsock and would arrive truncated.
    const BUFFER_SIZE: usize = 256 * 1024;
    const RECEIVE_BUFFER_BYTES: i32 = 16 * 1024 * 1024;
    const MAX_INTERFACES: usize = 8;

    fn ensure_winsock() -> Result<()> {
        static STARTED: OnceLock<i32> = OnceLock::new();
        let code = *STARTED.get_or_init(|| {
            let mut data: WSADATA = unsafe { std::mem::zeroed() };
            unsafe { WSAStartup(0x0202, &mut data) }
        });
        if code != 0 {
            bail!("WSAStartup failed with error {code}");
        }
        Ok(())
    }

    /// Local IPv4 addresses worth listening on. Loopback and link-local carry no
    /// game traffic, so they are skipped.
    fn local_ipv4_addresses() -> Result<Vec<Ipv4Addr>> {
        ensure_winsock()?;
        let mut name = [0u8; 256];
        if unsafe { gethostname(name.as_mut_ptr(), name.len() as i32) } == SOCKET_ERROR {
            bail!("gethostname failed with error {}", unsafe {
                WSAGetLastError()
            });
        }
        let hints = ADDRINFOA {
            ai_family: AF_INET as i32,
            ..Default::default()
        };
        let mut result: *mut ADDRINFOA = null_mut();
        let code = unsafe { getaddrinfo(name.as_ptr(), null(), &hints, &mut result) };
        if code != 0 {
            bail!("resolving local interfaces failed with error {code}");
        }
        let mut addresses = Vec::new();
        let mut cursor = result;
        while !cursor.is_null() {
            let entry = unsafe { &*cursor };
            if entry.ai_family == AF_INET as i32 && !entry.ai_addr.is_null() {
                let sockaddr = entry.ai_addr.cast::<SOCKADDR_IN>();
                let raw = unsafe { (*sockaddr).sin_addr.S_un.S_addr };
                let address = Ipv4Addr::from(u32::from_be(raw));
                if !address.is_loopback()
                    && !address.is_link_local()
                    && !address.is_unspecified()
                    && !addresses.contains(&address)
                {
                    addresses.push(address);
                }
            }
            cursor = entry.ai_next;
        }
        unsafe { freeaddrinfo(result) };
        addresses.truncate(MAX_INTERFACES);
        if addresses.is_empty() {
            bail!("no usable IPv4 interface was found for raw-socket capture");
        }
        Ok(addresses)
    }

    fn open_socket(address: Ipv4Addr) -> Result<SOCKET> {
        let handle = unsafe { socket(AF_INET as i32, SOCK_RAW, IPPROTO_IP) };
        if handle == INVALID_SOCKET {
            let error = unsafe { WSAGetLastError() };
            bail!(
                "opening a raw socket failed with error {error}; raw capture needs Administrator rights"
            );
        }
        let bound = SOCKADDR_IN {
            sin_family: AF_INET as ADDRESS_FAMILY,
            sin_port: 0,
            sin_addr: IN_ADDR {
                S_un: IN_ADDR_0 {
                    S_addr: u32::from(address).to_be(),
                },
            },
            sin_zero: [0; 8],
        };
        if unsafe {
            bind(
                handle,
                (&bound as *const SOCKADDR_IN).cast::<SOCKADDR>(),
                size_of::<SOCKADDR_IN>() as i32,
            )
        } == SOCKET_ERROR
        {
            let error = unsafe { WSAGetLastError() };
            unsafe { closesocket(handle) };
            bail!("binding a raw socket to {address} failed with error {error}");
        }
        let size = RECEIVE_BUFFER_BYTES;
        unsafe {
            setsockopt(
                handle,
                SOL_SOCKET,
                SO_RCVBUF,
                (&size as *const i32).cast(),
                size_of::<i32>() as i32,
            )
        };
        // Which SIO_RCVALL mode actually delivers both directions turns out to
        // be machine-specific, so it stays overridable while we measure.
        let mut enable: u32 = std::env::var("ARC_LIVE_RCVALL_MODE")
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok())
            .filter(|mode| *mode <= 3)
            .unwrap_or(RCVALL_IPLEVEL);
        let mut returned: u32 = 0;
        if unsafe {
            WSAIoctl(
                handle,
                SIO_RCVALL,
                (&enable as *const u32).cast(),
                size_of::<u32>() as u32,
                null_mut(),
                0,
                &mut returned,
                null_mut(),
                None,
            )
        } == SOCKET_ERROR
        {
            let error = unsafe { WSAGetLastError() };
            unsafe { closesocket(handle) };
            bail!(
                "enabling raw capture (SIO_RCVALL) on {address} failed with error {error}; Administrator rights are required"
            );
        }
        let mut non_blocking: u32 = 1;
        unsafe { ioctlsocket(handle, FIONBIO, &mut non_blocking) };
        enable = 0;
        let _ = enable;
        Ok(handle)
    }

    #[derive(Clone)]
    pub struct RawCaptureControl {
        stop: Arc<AtomicBool>,
        sockets: Arc<Mutex<Vec<SOCKET>>>,
    }

    impl RawCaptureControl {
        pub fn shutdown(&self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Ok(mut sockets) = self.sockets.lock() {
                for handle in sockets.drain(..) {
                    unsafe { closesocket(handle) };
                }
            }
        }
    }

    pub struct RawCapture {
        stop: Arc<AtomicBool>,
        sockets: Arc<Mutex<Vec<SOCKET>>>,
        next: usize,
        buffer: Vec<u8>,
        pub oversized_packets: u64,
    }

    impl RawCapture {
        pub fn open() -> Result<(Self, RawCaptureControl)> {
            let addresses = local_ipv4_addresses()?;
            let mut handles = Vec::new();
            let mut last_error = None;
            for address in addresses {
                match open_socket(address) {
                    Ok(handle) => handles.push(handle),
                    Err(error) => last_error = Some(error),
                }
            }
            if handles.is_empty() {
                return Err(last_error.unwrap_or_else(|| {
                    anyhow::anyhow!("no interface accepted a raw capture socket")
                }));
            }
            let stop = Arc::new(AtomicBool::new(false));
            let sockets = Arc::new(Mutex::new(handles));
            let control = RawCaptureControl {
                stop: Arc::clone(&stop),
                sockets: Arc::clone(&sockets),
            };
            Ok((
                Self {
                    stop,
                    sockets,
                    next: 0,
                    buffer: vec![0; BUFFER_SIZE],
                    oversized_packets: 0,
                },
                control,
            ))
        }

        /// Polls every interface once. Returns `None` when nothing is pending,
        /// so the caller can sleep instead of spinning.
        pub fn next_packet(&mut self) -> Result<Option<&[u8]>> {
            if self.stop.load(Ordering::Relaxed) {
                return Ok(None);
            }
            let handles: Vec<SOCKET> = match self.sockets.lock() {
                Ok(sockets) => sockets.clone(),
                Err(_) => return Ok(None),
            };
            if handles.is_empty() {
                return Ok(None);
            }
            for offset in 0..handles.len() {
                let index = (self.next + offset) % handles.len();
                let handle = handles[index];
                let read = unsafe {
                    recv(
                        handle,
                        self.buffer.as_mut_ptr(),
                        self.buffer.len() as i32,
                        0,
                    )
                };
                if read > 0 {
                    self.next = (index + 1) % handles.len();
                    return Ok(Some(&self.buffer[..read as usize]));
                }
                if read == SOCKET_ERROR {
                    let error = unsafe { WSAGetLastError() };
                    match error {
                        WSAEWOULDBLOCK => continue,
                        // Winsock filled the buffer and threw the rest away; a
                        // truncated segment would corrupt stream reassembly, so
                        // it is counted and skipped instead of forwarded.
                        WSAEMSGSIZE => {
                            self.oversized_packets = self.oversized_packets.saturating_add(1);
                            continue;
                        }
                        _ if self.stop.load(Ordering::Relaxed) => return Ok(None),
                        _ => continue,
                    }
                }
            }
            self.next = (self.next + 1) % handles.len();
            Ok(None)
        }
    }

    impl Drop for RawCapture {
        fn drop(&mut self) {
            if let Ok(mut sockets) = self.sockets.lock() {
                for handle in sockets.drain(..) {
                    unsafe { closesocket(handle) };
                }
            }
        }
    }

    /// The interfaces the capture binds to. Exposed for the probe example so a
    /// wrong interface choice can be told apart from a wrong socket mode.
    pub fn capture_addresses() -> Result<Vec<Ipv4Addr>> {
        local_ipv4_addresses()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn winsock_initialises_once() {
            ensure_winsock().unwrap();
            ensure_winsock().unwrap();
        }

        #[test]
        fn finds_a_local_interface_or_explains_why_not() {
            match local_ipv4_addresses() {
                Ok(addresses) => {
                    assert!(!addresses.is_empty());
                    assert!(addresses.iter().all(|address| !address.is_loopback()));
                    assert!(addresses.len() <= MAX_INTERFACES);
                }
                Err(error) => {
                    // Acceptable on a machine with no routable IPv4 at all.
                    assert!(error.to_string().contains("IPv4"));
                }
            }
        }
    }
}

#[cfg(windows)]
pub use platform::{RawCapture, RawCaptureControl, capture_addresses};

#[cfg(not(windows))]
pub fn capture_addresses() -> anyhow::Result<Vec<std::net::Ipv4Addr>> {
    Ok(Vec::new())
}

#[cfg(not(windows))]
#[derive(Clone)]
pub struct RawCaptureControl;

#[cfg(not(windows))]
impl RawCaptureControl {
    pub fn shutdown(&self) {}
}

#[cfg(not(windows))]
pub struct RawCapture;

#[cfg(not(windows))]
impl RawCapture {
    pub fn open() -> anyhow::Result<(Self, RawCaptureControl)> {
        anyhow::bail!("raw-socket capture is implemented for Windows only")
    }

    pub fn next_packet(&mut self) -> anyhow::Result<Option<&[u8]>> {
        Ok(None)
    }
}
