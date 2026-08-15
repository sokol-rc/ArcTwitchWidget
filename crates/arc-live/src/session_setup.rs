use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use arc_live_core::paths::AppPaths;

const VARIABLE: &str = "SSLKEYLOGFILE";

pub struct SessionSetup {
    pub keylog_path: PathBuf,
    pub launcher_ready: bool,
    pub status: String,
}

pub fn configure(paths: &AppPaths) -> Result<SessionSetup> {
    for launcher in ["steam.exe", "EpicGamesLauncher.exe"] {
        for value in crate::process_env::environment_values(launcher, VARIABLE) {
            let path = PathBuf::from(value);
            if usable_or_creatable(&path) {
                ensure_keylog(&path)?;
                return Ok(SessionSetup {
                    keylog_path: path,
                    launcher_ready: true,
                    status: format!(
                        "Automatic connection: {} already has TLS capture enabled",
                        launcher.trim_end_matches(".exe")
                    ),
                });
            }
        }
    }

    if let Some(path) = configured_user_path().filter(|path| usable_or_creatable(path)) {
        ensure_keylog(&path)?;
        return Ok(SessionSetup {
            keylog_path: path,
            launcher_ready: false,
            status: "Automatic TLS capture is installed; it activates after the launcher's next normal start"
                .to_owned(),
        });
    }

    let path = paths.sessions.join("arc-live-tls.keys");
    ensure_keylog(&path)?;
    install_user_path(&path)?;
    Ok(SessionSetup {
        keylog_path: path,
        launcher_ready: false,
        status: "One-time automatic TLS capture setup installed; no launcher or game was stopped"
            .to_owned(),
    })
}

fn usable_or_creatable(path: &Path) -> bool {
    path.is_file()
        || (!path.exists()
            && path
                .parent()
                .is_some_and(|parent| parent.exists() && parent.is_dir()))
}

fn ensure_keylog(path: &Path) -> Result<()> {
    if !path.exists() {
        fs::write(path, b"").with_context(|| format!("creating TLS keylog {}", path.display()))?;
    }
    Ok(())
}

#[cfg(windows)]
fn configured_user_path() -> Option<PathBuf> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let output = Command::new("reg.exe")
        .args(["query", r"HKCU\Environment", "/v", VARIABLE])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let value = text.lines().find(|line| line.contains(VARIABLE))?;
    let path = value.split("REG_SZ").nth(1)?.trim();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

#[cfg(not(windows))]
fn configured_user_path() -> Option<PathBuf> {
    std::env::var_os(VARIABLE).map(PathBuf::from)
}

#[cfg(windows)]
fn install_user_path(path: &Path) -> Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let output = Command::new("setx.exe")
        .arg(VARIABLE)
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .context("installing automatic TLS capture environment")?;
    anyhow::ensure!(
        output.status.success(),
        "installing automatic TLS capture failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

#[cfg(not(windows))]
fn install_user_path(_path: &Path) -> Result<()> {
    Ok(())
}

/// A launcher ARC Live can restart with the key log variable already set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Launcher {
    Steam,
    Epic,
}

impl Launcher {
    pub fn process_name(self) -> &'static str {
        match self {
            Self::Steam => "steam.exe",
            Self::Epic => "EpicGamesLauncher.exe",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Steam => "Steam",
            Self::Epic => "Epic Games Launcher",
        }
    }
}

/// Finds the installed launchers, so the app only offers what exists.
#[cfg(windows)]
pub fn installed_launchers() -> Vec<(Launcher, PathBuf)> {
    let mut found = Vec::new();
    for (launcher, key, value, relative) in [
        (
            Launcher::Steam,
            r"HKCU\Software\Valve\Steam",
            "SteamExe",
            None,
        ),
        (
            Launcher::Epic,
            r"HKLM\SOFTWARE\WOW6432Node\Epic Games\EpicGamesLauncher",
            "AppDataPath",
            Some(r"..\..\Launcher\Portal\Binaries\Win32\EpicGamesLauncher.exe"),
        ),
    ] {
        let Some(raw) = registry_value(key, value) else {
            continue;
        };
        let path = match relative {
            Some(relative) => PathBuf::from(raw).join(relative),
            None => PathBuf::from(raw.replace('/', "\\")),
        };
        if path.exists() {
            found.push((launcher, path));
        }
    }
    found
}

#[cfg(not(windows))]
pub fn installed_launchers() -> Vec<(Launcher, PathBuf)> {
    Vec::new()
}

#[cfg(windows)]
fn registry_value(key: &str, value: &str) -> Option<String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let output = Command::new("reg.exe")
        .args(["query", key, "/v", value])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().find(|line| line.contains(value))?;
    let path = line.split("REG_SZ").nth(1)?.trim();
    (!path.is_empty()).then(|| path.to_owned())
}

/// Closes the launcher and starts it again with `SSLKEYLOGFILE` in its
/// environment, so the game it launches inherits the variable. This is the only
/// way to make capture work without a full Windows re-login, and it happens
/// only when the user presses the button.
#[cfg(windows)]
pub fn restart_launcher_with_keylog(
    launcher: Launcher,
    executable: &Path,
    keylog_path: &Path,
) -> Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    ensure_keylog(keylog_path)?;
    let running =
        crate::process_env::environment_probe(launcher.process_name(), VARIABLE).processes;
    if running > 0 {
        match launcher {
            Launcher::Steam => {
                let _ = Command::new(executable)
                    .arg("-shutdown")
                    .creation_flags(CREATE_NO_WINDOW)
                    .status();
            }
            Launcher::Epic => {
                let _ = Command::new("taskkill.exe")
                    .args(["/IM", launcher.process_name(), "/F"])
                    .creation_flags(CREATE_NO_WINDOW)
                    .status();
            }
        }
        for _ in 0..40 {
            std::thread::sleep(std::time::Duration::from_millis(250));
            if crate::process_env::environment_probe(launcher.process_name(), VARIABLE).processes
                == 0
            {
                break;
            }
        }
    }
    Command::new(executable)
        .env(VARIABLE, keylog_path)
        .spawn()
        .with_context(|| format!("starting {}", executable.display()))?;
    Ok(())
}

#[cfg(not(windows))]
pub fn restart_launcher_with_keylog(
    _launcher: Launcher,
    _executable: &Path,
    _keylog_path: &Path,
) -> Result<()> {
    anyhow::bail!("launcher restart is supported on Windows only")
}
