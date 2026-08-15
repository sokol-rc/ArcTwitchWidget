use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use arc_live_core::state::GameKeylogStatus;
use crossbeam_channel::{Receiver, bounded};

const KEYLOG_VARIABLE: &str = "SSLKEYLOGFILE";

/// What the monitor sees about the game right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameStatus {
    pub running: bool,
    pub keylog: GameKeylogStatus,
    /// Where the game writes keys, when that is not our own file.
    pub keylog_path: Option<PathBuf>,
}

pub struct ProcessMonitor {
    pub changes: Receiver<GameStatus>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl ProcessMonitor {
    pub fn start(process_names: Vec<String>, expected_keylog: PathBuf) -> Self {
        let (tx, changes) = bounded(1);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            let mut previous: Option<GameStatus> = None;
            while !worker_stop.load(Ordering::Relaxed) {
                let running = game_is_running(&process_names);
                let status = if running {
                    game_keylog_status(&process_names, &expected_keylog)
                } else {
                    GameStatus {
                        running: false,
                        keylog: GameKeylogStatus::Unknown,
                        keylog_path: None,
                    }
                };
                if previous.as_ref() != Some(&status) {
                    let _ = tx.try_send(status.clone());
                    previous = Some(status);
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

/// Reads `SSLKEYLOGFILE` from the running game and compares it with the file
/// ARC Live decrypts from. A game started by a launcher that predates the
/// variable simply has none, and then nothing can ever be decrypted.
fn game_keylog_status(process_names: &[String], expected: &Path) -> GameStatus {
    let mut readable = false;
    let mut configured = None;
    for name in process_names {
        let probe = crate::process_env::environment_probe(name, KEYLOG_VARIABLE);
        readable |= probe.readable;
        if let Some(value) = probe.value {
            configured = Some(value);
            break;
        }
    }
    let Some(configured) = configured else {
        // An unreadable process (anti-cheat blocks the read) is not evidence
        // that the variable is missing, so stay silent instead of alarming.
        return GameStatus {
            running: true,
            keylog: if readable {
                GameKeylogStatus::Missing
            } else {
                GameKeylogStatus::Unknown
            },
            keylog_path: None,
        };
    };
    let path = PathBuf::from(configured.trim());
    if same_path(&path, expected) {
        GameStatus {
            running: true,
            keylog: GameKeylogStatus::Matches,
            keylog_path: None,
        }
    } else {
        GameStatus {
            running: true,
            keylog: GameKeylogStatus::Different,
            keylog_path: Some(path),
        }
    }
}

/// Windows paths are case-insensitive and the game may use a different form of
/// the same path, so compare canonical forms and fall back to a loose compare.
fn same_path(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => {
            left.to_string_lossy()
                .to_ascii_lowercase()
                .replace('/', "\\")
                == right
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .replace('/', "\\")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unreadable_process_is_not_reported_as_missing() {
        // Nothing matches this name, so no environment could be read at all.
        let status = game_keylog_status(&["definitely-not-running.exe".to_owned()], Path::new("x"));
        assert_eq!(status.keylog, GameKeylogStatus::Unknown);
        assert!(status.keylog_path.is_none());
    }

    #[cfg(windows)]
    #[test]
    fn own_process_environment_is_readable_and_compared() {
        let executable = std::env::current_exe().unwrap();
        let name = executable
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let probe = crate::process_env::environment_probe(&name, "PATH");
        assert!(probe.readable, "own process must be readable");
        assert!(probe.value.is_some(), "PATH is always set");
    }

    #[test]
    fn paths_compare_case_insensitively() {
        assert!(same_path(
            Path::new(r"C:\Users\Demo\ARC Live\arc-live-tls.keys"),
            Path::new(r"c:\users\demo\arc live\arc-live-tls.keys"),
        ));
        assert!(!same_path(
            Path::new(r"C:\Users\Demo\other.keys"),
            Path::new(r"C:\Users\Demo\arc-live-tls.keys"),
        ));
    }
}
