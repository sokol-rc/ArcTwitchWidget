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
