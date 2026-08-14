#[cfg(windows)]
mod platform {
    use std::ffi::{CString, c_char, c_void};
    use std::path::{Path, PathBuf};
    use std::ptr::null_mut;
    use std::sync::Arc;

    use anyhow::{Context, Result, bail};
    use libloading::Library;
    use windows_sys::Win32::Foundation::{GetLastError, HANDLE, INVALID_HANDLE_VALUE};

    const WINDIVERT_LAYER_NETWORK: i32 = 0;
    const WINDIVERT_FLAG_SNIFF: u64 = 0x0001;
    const WINDIVERT_FLAG_RECV_ONLY: u64 = 0x0004;
    const WINDIVERT_SHUTDOWN_RECV: i32 = 0x1;
    const BUFFER_SIZE: usize = 65_575;
    const FILTER: &str = "ip and tcp and (tcp.SrcPort == 443 or tcp.DstPort == 443)";

    type OpenFn = unsafe extern "system" fn(*const c_char, i32, i16, u64) -> HANDLE;
    type RecvFn = unsafe extern "system" fn(HANDLE, *mut c_void, u32, *mut u32, *mut c_void) -> i32;
    type ShutdownFn = unsafe extern "system" fn(HANDLE, i32) -> i32;
    type CloseFn = unsafe extern "system" fn(HANDLE) -> i32;

    struct WinDivertApi {
        _library: Library,
        open: OpenFn,
        recv: RecvFn,
        shutdown: ShutdownFn,
        close: CloseFn,
    }

    impl WinDivertApi {
        fn load(path: &Path) -> Result<Self> {
            let library = unsafe { Library::new(path) }
                .with_context(|| format!("loading {}", path.display()))?;
            let open = unsafe { *library.get::<OpenFn>(b"WinDivertOpen\0")? };
            let recv = unsafe { *library.get::<RecvFn>(b"WinDivertRecv\0")? };
            let shutdown = unsafe { *library.get::<ShutdownFn>(b"WinDivertShutdown\0")? };
            let close = unsafe { *library.get::<CloseFn>(b"WinDivertClose\0")? };
            Ok(Self {
                _library: library,
                open,
                recv,
                shutdown,
                close,
            })
        }
    }

    #[derive(Clone)]
    pub struct RawCaptureControl {
        api: Arc<WinDivertApi>,
        handle: isize,
    }

    impl RawCaptureControl {
        pub fn shutdown(&self) {
            unsafe {
                (self.api.shutdown)(self.handle as HANDLE, WINDIVERT_SHUTDOWN_RECV);
            }
        }
    }

    pub struct RawCapture {
        api: Arc<WinDivertApi>,
        handle: isize,
        buffer: Vec<u8>,
    }

    impl RawCapture {
        pub fn open() -> Result<(Self, RawCaptureControl)> {
            let dll_path = find_runtime_file("WinDivert.dll")?;
            let driver_path = dll_path.with_file_name("WinDivert64.sys");
            if !driver_path.exists() {
                bail!("WinDivert64.sys is missing next to {}", dll_path.display());
            }
            let api = Arc::new(WinDivertApi::load(&dll_path)?);
            let filter = CString::new(FILTER).expect("static WinDivert filter contains no NUL");
            let handle = unsafe {
                (api.open)(
                    filter.as_ptr(),
                    WINDIVERT_LAYER_NETWORK,
                    0,
                    WINDIVERT_FLAG_SNIFF | WINDIVERT_FLAG_RECV_ONLY,
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                let code = unsafe { GetLastError() };
                bail!(
                    "WinDivertOpen failed with Windows error {code}; verify Administrator access and security software policy"
                );
            }
            let control = RawCaptureControl {
                api: Arc::clone(&api),
                handle: handle as isize,
            };
            Ok((
                Self {
                    api,
                    handle: handle as isize,
                    buffer: vec![0; BUFFER_SIZE],
                },
                control,
            ))
        }

        pub fn next_packet(&mut self) -> Result<Option<&[u8]>> {
            let mut received = 0u32;
            let success = unsafe {
                (self.api.recv)(
                    self.handle as HANDLE,
                    self.buffer.as_mut_ptr().cast(),
                    self.buffer.len() as u32,
                    &mut received,
                    null_mut(),
                )
            };
            if success == 0 {
                let code = unsafe { GetLastError() };
                if code == 995 {
                    return Ok(None);
                }
                bail!("WinDivertRecv failed with Windows error {code}");
            }
            Ok(Some(&self.buffer[..received as usize]))
        }
    }

    impl Drop for RawCapture {
        fn drop(&mut self) {
            unsafe {
                (self.api.close)(self.handle as HANDLE);
            }
        }
    }

    fn find_runtime_file(name: &str) -> Result<PathBuf> {
        let mut candidates = Vec::new();
        if let Ok(directory) = std::env::var("ARC_LIVE_WINDIVERT_DIR") {
            candidates.push(PathBuf::from(directory).join(name));
        }
        if let Ok(executable) = std::env::current_exe()
            && let Some(directory) = executable.parent()
        {
            candidates.push(directory.join(name));
        }
        candidates.push(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("vendor")
                .join("windivert")
                .join("WinDivert-2.2.2-A")
                .join("x64")
                .join(name),
        );
        candidates
            .into_iter()
            .find(|path| path.exists())
            .ok_or_else(|| anyhow::anyhow!("{name} was not found next to ARC Live"))
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
        anyhow::bail!("WinDivert capture is currently implemented for Windows only")
    }

    pub fn next_packet(&mut self) -> anyhow::Result<Option<&[u8]>> {
        Ok(None)
    }
}
