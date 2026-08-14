use anyhow::Result;

#[cfg(windows)]
pub struct SingleInstanceGuard {
    handle: windows_sys::Win32::Foundation::HANDLE,
    pub activation: crossbeam_channel::Receiver<()>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

#[cfg(not(windows))]
pub struct SingleInstanceGuard {
    pub activation: crossbeam_channel::Receiver<()>,
}

#[cfg(windows)]
pub fn acquire() -> Result<Option<SingleInstanceGuard>> {
    use std::ptr::null;

    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONINFORMATION, MB_OK, MessageBoxW};

    let name = wide("Local\\ArcLive.Consumer.App");
    let handle = unsafe { CreateMutexW(null(), 0, name.as_ptr()) };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error().into());
    }
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe { CloseHandle(handle) };
        if let Ok(socket) = std::net::UdpSocket::bind("127.0.0.1:0")
            && socket.send_to(b"show", "127.0.0.1:17844").is_ok()
        {
            return Ok(None);
        }
        let message = wide("ARC Live уже запущена. Откройте её через значок рядом с часами.");
        let title = wide("ARC Live");
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                message.as_ptr(),
                title.as_ptr(),
                MB_OK | MB_ICONINFORMATION,
            )
        };
        return Ok(None);
    }
    let socket = std::net::UdpSocket::bind("127.0.0.1:17844")?;
    socket.set_read_timeout(Some(std::time::Duration::from_millis(500)))?;
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker_stop = std::sync::Arc::clone(&stop);
    let (tx, activation) = crossbeam_channel::bounded(1);
    let worker = std::thread::spawn(move || {
        let mut buffer = [0u8; 16];
        while !worker_stop.load(std::sync::atomic::Ordering::Relaxed) {
            match socket.recv_from(&mut buffer) {
                Ok((length, _)) if &buffer[..length] == b"show" => {
                    let _ = tx.try_send(());
                }
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => break,
            }
        }
    });
    Ok(Some(SingleInstanceGuard {
        handle,
        activation,
        stop,
        worker: Some(worker),
    }))
}

#[cfg(not(windows))]
pub fn acquire() -> Result<Option<SingleInstanceGuard>> {
    let (_tx, activation) = crossbeam_channel::bounded(1);
    Ok(Some(SingleInstanceGuard { activation }))
}

#[cfg(windows)]
impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Ok(socket) = std::net::UdpSocket::bind("127.0.0.1:0") {
            let _ = socket.send_to(b"stop", "127.0.0.1:17844");
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

#[cfg(windows)]
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
