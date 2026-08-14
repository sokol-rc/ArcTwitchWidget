use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, bounded};

pub struct ProcessMonitor {
    pub changes: Receiver<bool>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl ProcessMonitor {
    pub fn start(process_names: Vec<String>) -> Self {
        let (tx, changes) = bounded(1);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            let mut previous = None;
            while !worker_stop.load(Ordering::Relaxed) {
                let running = game_is_running(&process_names);
                if previous != Some(running) {
                    let _ = tx.try_send(running);
                    previous = Some(running);
                }
                for _ in 0..50 {
                    if worker_stop.load(Ordering::Relaxed) {
                        return;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }
        });
        Self {
            changes,
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for ProcessMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(windows)]
fn game_is_running(process_names: &[String]) -> bool {
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    let wanted: Vec<String> = process_names
        .iter()
        .map(|name| name.to_ascii_lowercase())
        .collect();
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return false;
        }
        let mut entry: PROCESSENTRY32W = zeroed();
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut found = false;
        let mut has_entry = Process32FirstW(snapshot, &mut entry) != 0;
        while has_entry {
            let end = entry
                .szExeFile
                .iter()
                .position(|character| *character == 0)
                .unwrap_or(entry.szExeFile.len());
            let executable = String::from_utf16_lossy(&entry.szExeFile[..end]).to_ascii_lowercase();
            if wanted.contains(&executable) {
                found = true;
                break;
            }
            has_entry = Process32NextW(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
        found
    }
}

#[cfg(not(windows))]
fn game_is_running(_process_names: &[String]) -> bool {
    false
}
