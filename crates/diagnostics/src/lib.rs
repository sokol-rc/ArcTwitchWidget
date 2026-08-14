use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use arc_live_core::paths::AppPaths;
use arc_live_core::redaction::sanitize_json;
use arc_live_core::state::AppState;
use arc_live_storage::Storage;
use serde_json::json;
use zip::write::SimpleFileOptions;

pub fn export(paths: &AppPaths, state: &AppState, storage: &Storage) -> Result<PathBuf> {
    fs::create_dir_all(&paths.exports)?;
    let destination = paths.exports.join(format!(
        "arc-live-diagnostics-{}.zip",
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
    ));
    let file = File::create(&destination)
        .with_context(|| format!("creating {}", destination.display()))?;
    let mut archive = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    let mut state_value = serde_json::to_value(state)?;
    if let Some(object) = state_value.as_object_mut() {
        object.insert(
            "keylog_path".into(),
            serde_json::Value::String("[LOCAL_PATH_REDACTED]".into()),
        );
        if let Some(activity) = object
            .get_mut("activity")
            .and_then(|value| value.as_array_mut())
        {
            for item in activity {
                if let Some(message) = item.get_mut("message")
                    && message
                        .as_str()
                        .is_some_and(|text| text.contains(":\\") || text.contains(":/"))
                {
                    *message =
                        serde_json::Value::String("[MESSAGE_WITH_LOCAL_PATH_REDACTED]".into());
                }
            }
        }
    }
    add_json(
        &mut archive,
        "state.json",
        &sanitize_json(&state_value),
        options,
    )?;

    let observations = storage.recent_observations(1000)?;
    add_json(
        &mut archive,
        "observations.json",
        &sanitize_json(&serde_json::to_value(observations)?),
        options,
    )?;

    add_json(
        &mut archive,
        "environment.json",
        &json!({
            "app_version": env!("CARGO_PKG_VERSION"),
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "exported_at": chrono::Utc::now(),
            "security_note": "JWTs, cookies, TLS keys, response values and account identifiers are excluded.",
        }),
        options,
    )?;
    if let Ok(bytes) = fs::read(&paths.widget_config)
        && let Ok(config) = serde_json::from_slice::<serde_json::Value>(&bytes)
    {
        add_json(&mut archive, "widget-config.json", &config, options)?;
    }
    archive.finish()?;
    Ok(destination)
}

fn add_json<W: Write + std::io::Seek>(
    archive: &mut zip::ZipWriter<W>,
    name: &str,
    value: &serde_json::Value,
    options: SimpleFileOptions,
) -> Result<()> {
    archive.start_file(name, options)?;
    archive.write_all(serde_json::to_string_pretty(value)?.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    #[test]
    fn bundle_excludes_keylog_paths_and_secret_files() {
        let root =
            std::env::temp_dir().join(format!("arc-live-diag-{:016x}", rand::random::<u64>()));
        let paths = AppPaths::from_root(root.clone()).unwrap();
        std::fs::write(
            paths.sessions.join("tls-secret.keys"),
            "CLIENT_RANDOM top-secret",
        )
        .unwrap();
        std::fs::write(
            &paths.widget_config,
            serde_json::to_vec(&arc_live_core::widget_config::WidgetConfig::default()).unwrap(),
        )
        .unwrap();
        let storage = Storage::open(&paths.database).unwrap();
        let mut state = AppState::new("test", r"C:\Users\Alice\secret.keys");
        state.record("error", r"Could not open C:\Users\Alice\private.txt");
        let bundle = export(&paths, &state, &storage).unwrap();
        let mut zip = zip::ZipArchive::new(File::open(bundle).unwrap()).unwrap();
        assert!(zip.by_name("tls-secret.keys").is_err());
        let mut state_json = String::new();
        zip.by_name("state.json")
            .unwrap()
            .read_to_string(&mut state_json)
            .unwrap();
        assert!(!state_json.contains("Alice"));
        assert!(!state_json.contains("secret.keys"));
        assert!(!state_json.contains("private.txt"));
        assert!(zip.by_name("widget-config.json").is_ok());
        drop(zip);
        let _ = std::fs::remove_dir_all(root);
    }
}
