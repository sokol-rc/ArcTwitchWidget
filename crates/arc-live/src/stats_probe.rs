use std::io::Read;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use arc_live_core::redaction::json_shape;
use arc_live_core::state::OverlayStats;
use arc_live_core::stats::normalize_player_stats;
use arc_live_storage::Observation;
use chrono::Utc;

const ENDPOINT: &str = "https://api-gateway.europe.es-pio.net/v1/pioneer/stats/player-v2";
const MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;

pub struct ProbeResult {
    pub observation: Observation,
    pub overlay: OverlayStats,
    pub unknown_event_rows: u64,
}

pub fn probe(
    token: &str,
    headers: &[(String, String)],
    request_body: &[u8],
) -> Result<ProbeResult> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .context("building stats probe client")?;
    let mut request = client.post(ENDPOINT);
    for (name, value) in headers {
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .with_context(|| format!("invalid captured request header name: {name}"))?;
        let value = reqwest::header::HeaderValue::from_str(value)
            .context("invalid captured request header value")?;
        request = request.header(name, value);
    }
    let mut response = request
        .bearer_auth(token)
        .body(request_body.to_vec())
        .send()
        .context("sending read-only player stats probe")?;
    let status = response.status();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    if !status.is_success() {
        bail!("player stats probe returned HTTP {}", status.as_u16());
    }
    let mut body = Vec::new();
    response
        .by_ref()
        .take(MAX_RESPONSE_BYTES + 1)
        .read_to_end(&mut body)
        .context("reading player stats probe response")?;
    if body.len() > MAX_RESPONSE_BYTES as usize {
        bail!("player stats probe response exceeded 4 MiB safety limit");
    }
    let value: serde_json::Value =
        serde_json::from_slice(&body).context("player stats probe did not return JSON")?;
    let (overlay, unknown_event_rows) = normalize_player_stats(&value)?;
    Ok(ProbeResult {
        observation: Observation {
            id: 0,
            observed_at: Utc::now(),
            direction: "active_probe_response".to_owned(),
            host: "api-gateway.europe.es-pio.net".to_owned(),
            method: Some("POST".to_owned()),
            path: Some("/v1/pioneer/stats/player-v2".to_owned()),
            status: Some(status.as_u16()),
            content_type,
            shape: json_shape(&value, 0),
        },
        overlay,
        unknown_event_rows,
    })
}
