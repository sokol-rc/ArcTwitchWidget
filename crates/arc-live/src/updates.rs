use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use crossbeam_channel::{Receiver, Sender, bounded};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MAX_INSTALLER_BYTES: u64 = 300 * 1024 * 1024;
const MAX_UPDATE_MANIFEST_BYTES: u64 = 256 * 1024;
const UPDATE_PUBLIC_KEY: &str = "IEBJVM9pYDspM38dBuBrYxFBazTRTRQHd9SDQ/SFdyo=";

#[derive(Debug, Deserialize)]
struct SignedUpdateEnvelope {
    schema_version: u8,
    algorithm: String,
    payload: String,
    signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateManifest {
    pub schema_version: u8,
    pub channel: String,
    pub version: String,
    pub installer_url: String,
    pub sha256: String,
    pub size: u64,
    #[serde(default = "default_silent_args")]
    pub silent_args: String,
}

fn default_silent_args() -> String {
    "/passive /norestart".to_owned()
}

#[derive(Debug, Clone)]
pub enum UpdateEvent {
    Current,
    Available(UpdateManifest),
    Downloaded(PathBuf),
    Error(String),
}

pub struct UpdateManager {
    tx: Sender<UpdateEvent>,
    pub events: Receiver<UpdateEvent>,
    pub checking: bool,
    pub downloading: bool,
    pub available: Option<UpdateManifest>,
    pub downloaded: Option<PathBuf>,
    pub error: Option<String>,
}

impl UpdateManager {
    pub fn new() -> Self {
        let (tx, events) = bounded(8);
        Self {
            tx,
            events,
            checking: false,
            downloading: false,
            available: None,
            downloaded: None,
            error: None,
        }
    }

    pub fn check(&mut self, feed_url: String, channel: String) {
        if self.checking || feed_url.trim().is_empty() {
            return;
        }
        self.checking = true;
        self.error = None;
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = check_update(&feed_url, &channel, env!("CARGO_PKG_VERSION"));
            let event = match result {
                Ok(Some(manifest)) => UpdateEvent::Available(manifest),
                Ok(None) => UpdateEvent::Current,
                Err(error) => UpdateEvent::Error(format!("Update check failed: {error:#}")),
            };
            let _ = tx.send(event);
        });
    }

    pub fn download(&mut self, updates_directory: PathBuf) {
        if self.downloading {
            return;
        }
        let Some(manifest) = self.available.clone() else {
            return;
        };
        self.downloading = true;
        self.error = None;
        let tx = self.tx.clone();
        thread::spawn(move || {
            let event = match download_update(&manifest, &updates_directory) {
                Ok(path) => UpdateEvent::Downloaded(path),
                Err(error) => UpdateEvent::Error(format!("Update download failed: {error:#}")),
            };
            let _ = tx.send(event);
        });
    }

    pub fn drain(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            self.checking = false;
            match event {
                UpdateEvent::Current => {
                    self.available = None;
                    self.downloaded = None;
                    self.error = None;
                }
                UpdateEvent::Available(manifest) => {
                    self.available = Some(manifest);
                    self.error = None;
                }
                UpdateEvent::Downloaded(path) => {
                    self.downloading = false;
                    self.downloaded = Some(path);
                    self.error = None;
                }
                UpdateEvent::Error(error) => {
                    self.checking = false;
                    self.downloading = false;
                    self.error = Some(error);
                }
            }
        }
    }

    pub fn launch_downloaded_after_exit(&self) -> Result<()> {
        let path = self
            .downloaded
            .as_ref()
            .context("no downloaded update is ready")?;
        // Waits for this process to go away, installs, then brings the updated
        // application back so the user does not have to find it in the menu.
        let script = "$targetPid = [int]$env:ARC_LIVE_UPDATE_WAIT_PID; \
             Wait-Process -Id $targetPid -ErrorAction SilentlyContinue; \
             Start-Process -FilePath $env:ARC_LIVE_UPDATE_INSTALLER \
             -ArgumentList @('/passive','/norestart') -Wait; \
             Start-Sleep -Seconds 3; \
             $application = $env:ARC_LIVE_UPDATE_RELAUNCH; \
             if ($application -and (Test-Path -LiteralPath $application)) { \
             Start-Process -FilePath $application }";
        let relaunch = std::env::current_exe().unwrap_or_default();
        Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-WindowStyle",
                "Hidden",
                "-Command",
                script,
            ])
            .env("ARC_LIVE_UPDATE_WAIT_PID", std::process::id().to_string())
            .env("ARC_LIVE_UPDATE_INSTALLER", path)
            .env("ARC_LIVE_UPDATE_RELAUNCH", relaunch)
            .spawn()
            .with_context(|| format!("scheduling {} after ARC Live exits", path.display()))?;
        Ok(())
    }
}

fn check_update(
    feed_url: &str,
    channel: &str,
    current_version: &str,
) -> Result<Option<UpdateManifest>> {
    ensure!(
        feed_url.starts_with("https://") || cfg!(debug_assertions),
        "release update feed must use HTTPS"
    );
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;
    let mut response = client.get(feed_url).send()?.error_for_status()?;
    let mut body = Vec::new();
    response
        .by_ref()
        .take(MAX_UPDATE_MANIFEST_BYTES + 1)
        .read_to_end(&mut body)?;
    ensure!(
        body.len() <= MAX_UPDATE_MANIFEST_BYTES as usize,
        "update manifest is too large"
    );
    let manifest = parse_signed_manifest(&body)?;
    ensure!(manifest.schema_version == 1, "unsupported update manifest");
    ensure!(manifest.channel == channel, "update channel mismatch");
    ensure!(
        manifest.installer_url.starts_with("https://"),
        "installer URL must use HTTPS"
    );
    ensure!(manifest.sha256.len() == 64, "invalid update SHA-256");
    Ok(is_newer(&manifest.version, current_version).then_some(manifest))
}

fn download_update(manifest: &UpdateManifest, updates_directory: &Path) -> Result<PathBuf> {
    ensure!(
        manifest.size <= MAX_INSTALLER_BYTES,
        "installer is too large"
    );
    fs::create_dir_all(updates_directory)?;
    let final_path = updates_directory.join(format!("ARC-Live-Setup-{}.exe", manifest.version));
    let temporary_path = final_path.with_extension("exe.download");
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?;
    let mut response = client
        .get(&manifest.installer_url)
        .send()?
        .error_for_status()?;
    if let Some(length) = response.content_length() {
        ensure!(length <= MAX_INSTALLER_BYTES, "installer is too large");
        if manifest.size > 0 {
            ensure!(
                length == manifest.size,
                "installer size does not match manifest"
            );
        }
    }
    let mut file = File::create(&temporary_path)?;
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = response.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total += read as u64;
        if total > MAX_INSTALLER_BYTES {
            let _ = fs::remove_file(&temporary_path);
            bail!("installer exceeded download limit");
        }
        hasher.update(&buffer[..read]);
        file.write_all(&buffer[..read])?;
    }
    file.sync_all()?;
    let actual_hash = format!("{:x}", hasher.finalize());
    if !actual_hash.eq_ignore_ascii_case(&manifest.sha256) {
        let _ = fs::remove_file(&temporary_path);
        bail!("installer SHA-256 does not match manifest");
    }
    if final_path.exists() {
        fs::remove_file(&final_path)?;
    }
    fs::rename(&temporary_path, &final_path)?;
    Ok(final_path)
}

fn parse_signed_manifest(body: &[u8]) -> Result<UpdateManifest> {
    parse_signed_manifest_with_key(body, UPDATE_PUBLIC_KEY)
}

fn parse_signed_manifest_with_key(body: &[u8], public_key: &str) -> Result<UpdateManifest> {
    let envelope: SignedUpdateEnvelope =
        serde_json::from_slice(body).context("parsing signed update envelope")?;
    ensure!(envelope.schema_version == 1, "unsupported update envelope");
    ensure!(
        envelope.algorithm == "ed25519",
        "unsupported update signature"
    );
    let payload = STANDARD
        .decode(envelope.payload)
        .context("decoding update payload")?;
    let signature_bytes = STANDARD
        .decode(envelope.signature)
        .context("decoding update signature")?;
    let signature = Signature::from_slice(&signature_bytes).context("invalid update signature")?;
    let public_key_bytes: [u8; 32] = STANDARD
        .decode(public_key)
        .context("decoding update public key")?
        .try_into()
        .map_err(|_| anyhow::anyhow!("update public key must contain 32 bytes"))?;
    let verifying_key =
        VerifyingKey::from_bytes(&public_key_bytes).context("invalid update public key")?;
    verifying_key
        .verify_strict(&payload, &signature)
        .context("update manifest signature is not valid")?;
    serde_json::from_slice(&payload).context("parsing verified update manifest")
}

fn is_newer(candidate: &str, current: &str) -> bool {
    let (candidate_core, candidate_suffix) = version_parts(candidate);
    let (current_core, current_suffix) = version_parts(current);
    if candidate_core != current_core {
        return candidate_core > current_core;
    }
    match (candidate_suffix.is_empty(), current_suffix.is_empty()) {
        (true, false) => true,
        (false, true) => false,
        _ => candidate_suffix > current_suffix,
    }
}

fn version_parts(version: &str) -> ((u64, u64, u64), String) {
    let mut core_and_suffix = version.splitn(2, '-');
    let core = core_and_suffix.next().unwrap_or_default();
    let suffix = core_and_suffix.next().unwrap_or_default().to_owned();
    let mut parts = core.split('.').map(|part| part.parse::<u64>().unwrap_or(0));
    (
        (
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
        ),
        suffix,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    #[test]
    fn compares_release_versions() {
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(!is_newer("0.10.0", "0.10.0"));
        assert!(!is_newer("0.9.9", "0.10.0"));
        assert!(is_newer("1.0.0", "1.0.0-beta.1"));
        assert!(!is_newer("1.0.0-beta.1", "1.0.0"));
    }

    #[test]
    fn manifest_never_contains_signing_material() {
        let manifest = UpdateManifest {
            schema_version: 1,
            channel: "stable".into(),
            version: "1.2.3".into(),
            installer_url: "https://example.test/ARC-Live-Setup.exe".into(),
            sha256: "a".repeat(64),
            size: 42,
            silent_args: default_silent_args(),
        };
        let value = serde_json::to_string(&manifest).unwrap();
        assert!(!value.contains("certificate"));
    }

    #[test]
    fn accepts_signed_manifest_and_rejects_tampering() {
        let signing_key = SigningKey::from_bytes(&[7; 32]);
        let payload = serde_json::to_vec(&UpdateManifest {
            schema_version: 1,
            channel: "stable".into(),
            version: "1.2.3".into(),
            installer_url: "https://example.test/ARC-Live-Setup.exe".into(),
            sha256: "a".repeat(64),
            size: 42,
            silent_args: default_silent_args(),
        })
        .unwrap();
        let envelope = serde_json::json!({
            "schema_version": 1,
            "algorithm": "ed25519",
            "payload": STANDARD.encode(&payload),
            "signature": STANDARD.encode(signing_key.sign(&payload).to_bytes()),
        });
        let public_key = STANDARD.encode(signing_key.verifying_key().to_bytes());
        let encoded = serde_json::to_vec(&envelope).unwrap();
        assert_eq!(
            parse_signed_manifest_with_key(&encoded, &public_key)
                .unwrap()
                .version,
            "1.2.3"
        );

        let mut tampered = envelope;
        tampered["payload"] = serde_json::Value::String(STANDARD.encode(b"{}"));
        assert!(
            parse_signed_manifest_with_key(&serde_json::to_vec(&tampered).unwrap(), &public_key)
                .is_err()
        );
    }
}
