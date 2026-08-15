//! Packet source built on the packet-capture provider that ships with Windows
//! (`Microsoft-Windows-NDIS-PacketCapture`, the one `netsh trace` uses).
//!
//! Unlike raw sockets it sees both directions on every machine tested, and
//! unlike WinDivert it needs no third-party driver: the kernel component is
//! `ndiscap.sys`, part of Windows and signed by Microsoft. Frames are consumed
//! straight from the real-time ETW session, so nothing raw is ever written to
//! disk - the project invariant an ETL/pcap detour would have broken.
//!
//! Administrator rights are still required, which the LocalSystem capture
//! service already has.

#[cfg(windows)]
mod platform {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;

    use anyhow::{Result, bail};
    use crossbeam_channel::{Receiver, Sender, bounded};
    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_SUCCESS};
    use windows_sys::Win32::System::Diagnostics::Etw::{
        CONTROLTRACE_HANDLE, CloseTrace, ControlTraceW, EVENT_CONTROL_CODE_ENABLE_PROVIDER,
        EVENT_RECORD, EVENT_TRACE_CONTROL_STOP, EVENT_TRACE_LOGFILEW, EVENT_TRACE_PROPERTIES,
        EVENT_TRACE_REAL_TIME_MODE, EnableTraceEx2, OpenTraceW, PROCESS_TRACE_MODE_EVENT_RECORD,
        PROCESS_TRACE_MODE_REAL_TIME, PROCESSTRACE_HANDLE, ProcessTrace, StartTraceW,
        WNODE_FLAG_TRACED_GUID,
    };
    use windows_sys::core::GUID;

    /// `Microsoft-Windows-NDIS-PacketCapture`.
    const NDISCAP: GUID = GUID {
        data1: 0x2ED6_006E,
        data2: 0x4729,
        data3: 0x4609,
        data4: [0xB4, 0x23, 0x3E, 0xE7, 0xBC, 0xD6, 0x78, 0xEF],
    };
    /// Both fragment events carry `MiniportIfIndex`, `LowerIfIndex`, the size
    /// and then the frame itself.
    const EVENT_FRAGMENT: [u16; 2] = [1001, 1002];
    const SESSION_NAME: &str = "ArcLiveNdisCapture";
    const FRAGMENT_HEADER: usize = 12;
    const ETHERNET_HEADER: usize = 14;
    const ETHERTYPE_IPV4: u16 = 0x0800;
    const ETHERTYPE_VLAN: u16 = 0x8100;
    const QUEUE_CAPACITY: usize = 8192;
    const TRACE_LEVEL_VERBOSE: u8 = 5;

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// `EVENT_TRACE_PROPERTIES` has to be followed by the session name in the
    /// same allocation, which is why this is a byte buffer and not a struct.
    struct SessionProperties {
        buffer: Vec<u8>,
    }

    impl SessionProperties {
        fn new(name: &str) -> Self {
            let name = wide(name);
            let header = size_of::<EVENT_TRACE_PROPERTIES>();
            let total = header + name.len() * 2;
            let mut buffer = vec![0u8; total];
            unsafe {
                let properties = buffer.as_mut_ptr().cast::<EVENT_TRACE_PROPERTIES>();
                (*properties).Wnode.BufferSize = total as u32;
                (*properties).Wnode.Flags = WNODE_FLAG_TRACED_GUID;
                (*properties).Wnode.ClientContext = 1; // QPC timestamps
                (*properties).BufferSize = 256;
                (*properties).MinimumBuffers = 16;
                (*properties).MaximumBuffers = 64;
                (*properties).LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
                (*properties).LoggerNameOffset = header as u32;
                std::ptr::copy_nonoverlapping(
                    name.as_ptr().cast::<u8>(),
                    buffer.as_mut_ptr().add(header),
                    name.len() * 2,
                );
            }
            Self { buffer }
        }

        fn as_mut_ptr(&mut self) -> *mut EVENT_TRACE_PROPERTIES {
            self.buffer.as_mut_ptr().cast()
        }
    }

    /// Starts the session, retrying once after stopping a session left behind
    /// by a previous crash.
    fn start_session() -> Result<(CONTROLTRACE_HANDLE, SessionProperties)> {
        let name = wide(SESSION_NAME);
        let mut properties = SessionProperties::new(SESSION_NAME);
        let mut handle: CONTROLTRACE_HANDLE = Default::default();
        let mut status =
            unsafe { StartTraceW(&mut handle, name.as_ptr(), properties.as_mut_ptr()) };
        if status == ERROR_ALREADY_EXISTS {
            let mut stale = SessionProperties::new(SESSION_NAME);
            unsafe {
                ControlTraceW(
                    Default::default(),
                    name.as_ptr(),
                    stale.as_mut_ptr(),
                    EVENT_TRACE_CONTROL_STOP,
                )
            };
            properties = SessionProperties::new(SESSION_NAME);
            status = unsafe { StartTraceW(&mut handle, name.as_ptr(), properties.as_mut_ptr()) };
        }
        if status != ERROR_SUCCESS {
            bail!(
                "starting the Windows packet-capture trace failed with error {status}; \
                 Administrator rights are required"
            );
        }
        let status = unsafe {
            EnableTraceEx2(
                handle,
                &NDISCAP,
                EVENT_CONTROL_CODE_ENABLE_PROVIDER,
                TRACE_LEVEL_VERBOSE,
                u64::MAX,
                0,
                0,
                std::ptr::null(),
            )
        };
        if status != ERROR_SUCCESS {
            let mut stop = SessionProperties::new(SESSION_NAME);
            unsafe {
                ControlTraceW(
                    handle,
                    std::ptr::null(),
                    stop.as_mut_ptr(),
                    EVENT_TRACE_CONTROL_STOP,
                )
            };
            bail!("enabling the packet-capture provider failed with error {status}");
        }
        Ok((handle, properties))
    }

    /// Receives one ETW event and forwards the IPv4 packet inside it.
    unsafe extern "system" fn on_event(record: *mut EVENT_RECORD) {
        let record = unsafe { &*record };
        if !EVENT_FRAGMENT.contains(&record.EventHeader.EventDescriptor.Id) {
            return;
        }
        if record.UserData.is_null() || record.UserContext.is_null() {
            return;
        }
        let data = unsafe {
            std::slice::from_raw_parts(record.UserData.cast::<u8>(), record.UserDataLength as usize)
        };
        if data.len() <= FRAGMENT_HEADER {
            return;
        }
        let declared = u32::from_le_bytes([data[8], data[9], data[10], data[11]]) as usize;
        let frame = &data[FRAGMENT_HEADER..];
        let frame = &frame[..declared.min(frame.len())];
        let Some(packet) = ipv4_payload(frame) else {
            return;
        };
        let sender = unsafe { &*record.UserContext.cast::<Sender<Vec<u8>>>() };
        // Dropping under pressure is better than blocking the ETW callback,
        // which would stall the whole session.
        let _ = sender.try_send(packet.to_vec());
    }

    /// Strips the Ethernet (and optional VLAN) header so the rest of the crate
    /// keeps seeing bare IPv4, exactly like the other backends deliver.
    fn ipv4_payload(frame: &[u8]) -> Option<&[u8]> {
        if frame.len() <= ETHERNET_HEADER {
            return None;
        }
        let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
        let (ethertype, offset) = if ethertype == ETHERTYPE_VLAN {
            if frame.len() <= ETHERNET_HEADER + 4 {
                return None;
            }
            (
                u16::from_be_bytes([frame[16], frame[17]]),
                ETHERNET_HEADER + 4,
            )
        } else {
            (ethertype, ETHERNET_HEADER)
        };
        (ethertype == ETHERTYPE_IPV4).then(|| &frame[offset..])
    }

    #[derive(Clone)]
    pub struct RawCaptureControl {
        stop: Arc<AtomicBool>,
        session: Arc<Mutex<CONTROLTRACE_HANDLE>>,
        trace: Arc<AtomicU64>,
    }

    impl RawCaptureControl {
        pub fn shutdown(&self) {
            if self.stop.swap(true, Ordering::SeqCst) {
                return;
            }
            let trace = self.trace.load(Ordering::SeqCst);
            if trace != 0 {
                unsafe { CloseTrace(PROCESSTRACE_HANDLE { Value: trace }) };
            }
            if let Ok(session) = self.session.lock() {
                let mut properties = SessionProperties::new(SESSION_NAME);
                unsafe {
                    ControlTraceW(
                        *session,
                        std::ptr::null(),
                        properties.as_mut_ptr(),
                        EVENT_TRACE_CONTROL_STOP,
                    )
                };
            }
        }
    }

    pub struct RawCapture {
        packets: Receiver<Vec<u8>>,
        current: Vec<u8>,
        control: RawCaptureControl,
        worker: Option<thread::JoinHandle<()>>,
    }

    impl RawCapture {
        pub fn open() -> Result<(Self, RawCaptureControl)> {
            let (session, _properties) = start_session()?;
            let (tx, packets) = bounded(QUEUE_CAPACITY);
            let control = RawCaptureControl {
                stop: Arc::new(AtomicBool::new(false)),
                session: Arc::new(Mutex::new(session)),
                trace: Arc::new(AtomicU64::new(0)),
            };
            let trace_slot = Arc::clone(&control.trace);
            let worker = thread::spawn(move || {
                // The sender outlives ProcessTrace: the callback dereferences
                // this pointer for every event.
                let sender = Box::into_raw(Box::new(tx));
                let name = wide(SESSION_NAME);
                let mut logfile: EVENT_TRACE_LOGFILEW = unsafe { std::mem::zeroed() };
                logfile.LoggerName = name.as_ptr().cast_mut();
                logfile.Anonymous1.ProcessTraceMode =
                    PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD;
                logfile.Anonymous2.EventRecordCallback = Some(on_event);
                logfile.Context = sender.cast();
                let trace = unsafe { OpenTraceW(&mut logfile) };
                // INVALID_PROCESSTRACE_HANDLE is all-ones on 64-bit Windows.
                if trace.Value == u64::MAX {
                    unsafe { drop(Box::from_raw(sender)) };
                    return;
                }
                trace_slot.store(trace.Value, Ordering::SeqCst);
                unsafe {
                    ProcessTrace(&trace, 1, std::ptr::null(), std::ptr::null());
                    CloseTrace(trace);
                    drop(Box::from_raw(sender));
                }
            });
            Ok((
                Self {
                    packets,
                    current: Vec::new(),
                    control: control.clone(),
                    worker: Some(worker),
                },
                control,
            ))
        }

        pub fn next_packet(&mut self) -> Result<Option<&[u8]>> {
            match self.packets.try_recv() {
                Ok(packet) => {
                    self.current = packet;
                    Ok(Some(&self.current))
                }
                Err(_) => Ok(None),
            }
        }
    }

    impl Drop for RawCapture {
        fn drop(&mut self) {
            self.control.shutdown();
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn strips_ethernet_and_vlan_headers() {
            let mut plain = vec![0u8; ETHERNET_HEADER + 4];
            plain[12] = 0x08;
            plain[13] = 0x00;
            plain[ETHERNET_HEADER] = 0x45;
            assert_eq!(ipv4_payload(&plain).unwrap()[0], 0x45);

            let mut tagged = vec![0u8; ETHERNET_HEADER + 8];
            tagged[12] = 0x81;
            tagged[13] = 0x00;
            tagged[16] = 0x08;
            tagged[17] = 0x00;
            tagged[ETHERNET_HEADER + 4] = 0x45;
            assert_eq!(ipv4_payload(&tagged).unwrap()[0], 0x45);
        }

        #[test]
        fn ignores_non_ipv4_frames() {
            let mut arp = vec![0u8; ETHERNET_HEADER + 4];
            arp[12] = 0x08;
            arp[13] = 0x06;
            assert!(ipv4_payload(&arp).is_none());
            assert!(ipv4_payload(&[0u8; 4]).is_none());
        }
    }
}

#[cfg(windows)]
pub use platform::{RawCapture, RawCaptureControl};

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
        anyhow::bail!("the Windows packet-capture provider is available on Windows only")
    }

    pub fn next_packet(&mut self) -> anyhow::Result<Option<&[u8]>> {
        Ok(None)
    }
}
