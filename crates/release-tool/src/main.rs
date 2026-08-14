use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signer, SigningKey};
use serde::Serialize;
use zeroize::Zeroizing;

#[derive(Serialize)]
struct SignedUpdateEnvelope {
    schema_version: u8,
    algorithm: &'static str,
    payload: String,
    signature: String,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("ARC Live release tool failed: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut arguments = std::env::args().skip(1);
    match arguments.next().as_deref() {
        Some("generate") => {
            let secret = arguments.next().context("missing secret output path")?;
            let public = arguments.next().context("missing public output path")?;
            ensure!(arguments.next().is_none(), "unexpected generate argument");
            generate(Path::new(&secret), Path::new(&public))
        }
        Some("sign") => {
            let input = arguments.next().context("missing manifest input path")?;
            let output = arguments
                .next()
                .context("missing signed manifest output path")?;
            ensure!(arguments.next().is_none(), "unexpected sign argument");
            sign(Path::new(&input), Path::new(&output))
        }
        _ => bail!(
            "usage: arc-live-release-tool generate <secret> <public> | sign <manifest> <output>"
        ),
    }
}

fn generate(secret_path: &Path, public_path: &Path) -> Result<()> {
    ensure!(
        !secret_path.exists() && !public_path.exists(),
        "refusing to overwrite an update key"
    );
    let signing_key = SigningKey::from_bytes(&rand::random());
    fs::write(secret_path, STANDARD.encode(signing_key.to_bytes()))
        .with_context(|| format!("writing {}", secret_path.display()))?;
    fs::write(
        public_path,
        STANDARD.encode(signing_key.verifying_key().to_bytes()),
    )
    .with_context(|| format!("writing {}", public_path.display()))?;
    Ok(())
}

fn sign(input: &Path, output: &Path) -> Result<()> {
    let secret = Zeroizing::new(
        std::env::var("ARC_LIVE_UPDATE_SIGNING_KEY")
            .context("ARC_LIVE_UPDATE_SIGNING_KEY is not set")?,
    );
    let decoded = Zeroizing::new(
        STANDARD
            .decode(secret.trim())
            .context("decoding update signing key")?,
    );
    let secret_bytes: [u8; 32] = decoded
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("update signing key must contain 32 bytes"))?;
    let signing_key = SigningKey::from_bytes(&secret_bytes);

    let source = fs::read(input).with_context(|| format!("reading {}", input.display()))?;
    let payload_value: serde_json::Value =
        serde_json::from_slice(&source).context("parsing unsigned update manifest")?;
    let payload = serde_json::to_vec(&payload_value)?;
    let signature = signing_key.sign(&payload);
    let envelope = SignedUpdateEnvelope {
        schema_version: 1,
        algorithm: "ed25519",
        payload: STANDARD.encode(payload),
        signature: STANDARD.encode(signature.to_bytes()),
    };
    fs::write(output, serde_json::to_vec_pretty(&envelope)?)
        .with_context(|| format!("writing {}", output.display()))?;
    Ok(())
}
