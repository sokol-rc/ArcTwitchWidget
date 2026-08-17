use std::fs;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

pub const DEFAULT_STABLE_UPDATE_FEED_URL: &str =
    "https://github.com/sokol-rc/ArcTwitchWidget/releases/latest/download/stable.json";
pub const DEFAULT_BETA_UPDATE_FEED_URL: &str =
    "https://github.com/sokol-rc/ArcTwitchWidget/releases/download/beta/beta.json";

/// Current revision of the risk notice shown before the application can be
/// used. Bump it whenever the wording changes materially.
pub const DISCLAIMER_REVISION: u8 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub local_port: u16,
    pub overlay_width: u16,
    pub overlay_height: u16,
    pub overlay_preset: String,
    pub overlay_language: String,
    pub overlay_background_preset: String,
    pub overlay_background_color: [u8; 3],
    pub overlay_opacity: u8,
    pub overlay_blur: u8,
    pub game_process_names: Vec<String>,
    pub onboarding_completed: bool,
    /// Which revision of the risk notice the user has confirmed. Raising
    /// [`DISCLAIMER_REVISION`] shows the notice again after an update.
    pub disclaimer_accepted_revision: u8,
    pub update_channel: String,
    pub automatic_updates: bool,
    pub update_feed_url: String,
    pub beta_update_feed_url: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            local_port: 17_842,
            overlay_width: 700,
            overlay_height: 80,
            overlay_preset: "account".into(),
            overlay_language: "ru".into(),
            overlay_background_preset: "smoke".into(),
            overlay_background_color: [9, 16, 21],
            overlay_opacity: 48,
            overlay_blur: 4,
            game_process_names: vec![
                "PioneerGame.exe".into(),
                "PioneerGame-Win64-Shipping.exe".into(),
                "ArcRaiders.exe".into(),
                "ArcRaiders-Win64-Shipping.exe".into(),
            ],
            onboarding_completed: false,
            disclaimer_accepted_revision: 0,
            update_channel: "stable".into(),
            automatic_updates: true,
            update_feed_url: option_env!("ARC_LIVE_UPDATE_FEED_URL")
                .unwrap_or(DEFAULT_STABLE_UPDATE_FEED_URL)
                .to_owned(),
            beta_update_feed_url: option_env!("ARC_LIVE_BETA_UPDATE_FEED_URL")
                .unwrap_or(DEFAULT_BETA_UPDATE_FEED_URL)
                .to_owned(),
        }
    }
}

impl AppConfig {
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if !path.exists() {
            let config = Self::default();
            fs::write(path, serde_json::to_vec_pretty(&config)?)
                .with_context(|| format!("creating {}", path.display()))?;
            return Ok(config);
        }
        let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let mut config: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", path.display()))?;
        let mut migrated = false;
        if config.update_feed_url.trim().is_empty() {
            config.update_feed_url = DEFAULT_STABLE_UPDATE_FEED_URL.to_owned();
            migrated = true;
        }
        if config.beta_update_feed_url.trim().is_empty() {
            config.beta_update_feed_url = DEFAULT_BETA_UPDATE_FEED_URL.to_owned();
            migrated = true;
        }
        config.validate()?;
        if migrated {
            config.save(path)?;
        }
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(self.local_port >= 1024, "local_port must be at least 1024");
        ensure!(
            self.overlay_width >= 320,
            "overlay_width must be at least 320"
        );
        ensure!(
            self.overlay_height >= 60,
            "overlay_height must be at least 60"
        );
        // Preset ids come from the user's widget-config.json, so only the shape
        // is validated here; a missing id falls back to the first preset.
        ensure!(
            !self.overlay_preset.trim().is_empty(),
            "overlay_preset is empty"
        );
        ensure!(
            matches!(self.overlay_language.as_str(), "ru" | "en"),
            "overlay_language is invalid"
        );
        ensure!(
            self.overlay_opacity <= 100,
            "overlay_opacity must be at most 100"
        );
        ensure!(self.overlay_blur <= 20, "overlay_blur must be at most 20");
        ensure!(
            matches!(self.update_channel.as_str(), "stable" | "beta"),
            "update_channel must be stable or beta"
        );
        ensure!(
            self.update_feed_url.starts_with("https://")
                && self.beta_update_feed_url.starts_with("https://"),
            "update feeds must use HTTPS"
        );
        ensure!(
            !self.game_process_names.is_empty(),
            "game_process_names must contain at least one executable"
        );
        Ok(())
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        fs::write(path, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("saving {}", path.display()))
    }

    pub fn selected_update_feed_url(&self) -> String {
        if self.update_channel == "beta" {
            self.beta_update_feed_url.clone()
        } else {
            self.update_feed_url.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_and_reloads_defaults() {
        let root =
            std::env::temp_dir().join(format!("arc-live-config-{:016x}", rand::random::<u64>()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("config.json");
        let first = AppConfig::load_or_create(&path).unwrap();
        let second = AppConfig::load_or_create(&path).unwrap();
        assert_eq!(first.local_port, 17_842);
        assert_eq!(second.local_port, first.local_port);
        assert_eq!(second.overlay_width, 700);
        assert_eq!(second.overlay_height, 80);
        assert_eq!(second.update_feed_url, DEFAULT_STABLE_UPDATE_FEED_URL);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn migrates_placeholder_update_channel_to_github_releases() {
        let root =
            std::env::temp_dir().join(format!("arc-live-update-{:016x}", rand::random::<u64>()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("config.json");
        let mut old = AppConfig::default();
        old.update_feed_url.clear();
        old.beta_update_feed_url.clear();
        std::fs::write(&path, serde_json::to_vec_pretty(&old).unwrap()).unwrap();

        let migrated = AppConfig::load_or_create(&path).unwrap();
        assert_eq!(
            migrated.selected_update_feed_url(),
            DEFAULT_STABLE_UPDATE_FEED_URL
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
