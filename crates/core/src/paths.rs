use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::ProjectDirs;

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub root: PathBuf,
    pub database: PathBuf,
    pub config: PathBuf,
    pub widget_config: PathBuf,
    pub legacy_widget_config: Option<PathBuf>,
    pub logs: PathBuf,
    pub sessions: PathBuf,
    pub exports: PathBuf,
    pub updates: PathBuf,
    pub service_token: PathBuf,
}

impl AppPaths {
    pub fn discover() -> Result<Self> {
        let dirs = ProjectDirs::from("io", "ArcLive", "ARC Live")
            .context("Windows did not provide a local application data directory")?;
        let mut paths = Self::from_root(dirs.data_local_dir().to_path_buf())?;
        paths.legacy_widget_config = std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(PathBuf::from))
            .map(|directory| directory.join("widget-config.json"))
            .filter(|path| path != &paths.widget_config);
        paths.migrate_legacy_widget_config()?;
        Ok(paths)
    }

    pub fn from_root(root: PathBuf) -> Result<Self> {
        let paths = Self {
            database: root.join("arc-live.sqlite3"),
            config: root.join("config.json"),
            widget_config: root.join("widget-config.json"),
            legacy_widget_config: None,
            logs: root.join("logs"),
            sessions: root.join("sessions"),
            exports: root.join("exports"),
            updates: root.join("updates"),
            service_token: root.join("service-token"),
            root,
        };
        for directory in [
            &paths.root,
            &paths.logs,
            &paths.sessions,
            &paths.exports,
            &paths.updates,
        ] {
            fs::create_dir_all(directory)
                .with_context(|| format!("creating {}", directory.display()))?;
        }
        Ok(paths)
    }

    pub fn new_keylog_path(&self) -> PathBuf {
        let suffix: u64 = rand::random();
        self.sessions.join(format!(
            "tls-{}-{suffix:016x}.keys",
            chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
        ))
    }

    pub fn load_or_create_service_token(&self) -> Result<String> {
        if self.service_token.exists() {
            let token = fs::read_to_string(&self.service_token)
                .with_context(|| format!("reading {}", self.service_token.display()))?;
            let token = token.trim().to_owned();
            anyhow::ensure!(token.len() == 64, "invalid local service token");
            return Ok(token);
        }
        let bytes: [u8; 32] = rand::random();
        let token = bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        fs::write(&self.service_token, &token)
            .with_context(|| format!("creating {}", self.service_token.display()))?;
        Ok(token)
    }

    fn migrate_legacy_widget_config(&self) -> Result<()> {
        if self.widget_config.exists() {
            return Ok(());
        }
        let Some(legacy) = self
            .legacy_widget_config
            .as_ref()
            .filter(|path| path.exists())
        else {
            return Ok(());
        };
        fs::copy(legacy, &self.widget_config).with_context(|| {
            format!(
                "migrating widget configuration from {} to {}",
                legacy.display(),
                self.widget_config.display()
            )
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_user_editable_widget_config_in_data_directory() {
        let root =
            std::env::temp_dir().join(format!("arc-live-paths-{:016x}", rand::random::<u64>()));
        let paths = AppPaths::from_root(root.clone()).unwrap();
        assert_eq!(paths.widget_config, root.join("widget-config.json"));
        assert!(paths.legacy_widget_config.is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn creates_stable_high_entropy_service_token() {
        let root =
            std::env::temp_dir().join(format!("arc-live-token-{:016x}", rand::random::<u64>()));
        let paths = AppPaths::from_root(root.clone()).unwrap();
        let first = paths.load_or_create_service_token().unwrap();
        let second = paths.load_or_create_service_token().unwrap();
        assert_eq!(first, second);
        assert_eq!(first.len(), 64);
        let _ = fs::remove_dir_all(root);
    }
}
