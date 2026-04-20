//! Runtime configuration for tier1.
//!
//! Defaults are product defaults. Environment variables allow quick tuning
//! without code edits.

use std::env;
use std::sync::OnceLock;

use crate::ThumbnailProfile;

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub thumbnail_width: u32,
    pub thumbnail_height: u32,
    pub thumbnail_quality: u8,
    pub pixel_art_thumbnail_quality: u8,
    pub vignette_strength: f32,
    pub developer_mode: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            thumbnail_width: 250,
            thumbnail_height: 200,
            thumbnail_quality: 46,
            pixel_art_thumbnail_quality: 18,
            vignette_strength: 0.25,
            developer_mode: false,
        }
    }
}

impl AppConfig {
    pub fn from_env() -> Self {
        let defaults = Self::default();
        Self {
            thumbnail_width: env_u32("THUMBRELLA_THUMBNAIL_WIDTH", defaults.thumbnail_width),
            thumbnail_height: env_u32("THUMBRELLA_THUMBNAIL_HEIGHT", defaults.thumbnail_height),
            thumbnail_quality: env_u8("THUMBRELLA_THUMBNAIL_QUALITY", defaults.thumbnail_quality),
            pixel_art_thumbnail_quality: env_u8(
                "THUMBRELLA_PIXEL_ART_THUMBNAIL_QUALITY",
                defaults.pixel_art_thumbnail_quality,
            ),
            vignette_strength: env_f32("THUMBRELLA_VIGNETTE_STRENGTH", defaults.vignette_strength),
            developer_mode: env_bool("THUMBRELLA_DEVELOPER_MODE", defaults.developer_mode),
        }
    }

    pub fn thumbnail_profile(&self) -> ThumbnailProfile {
        ThumbnailProfile {
            version: 1,
            width: self.thumbnail_width,
            height: self.thumbnail_height,
            quality: self.thumbnail_quality,
            pixel_art_quality: self.pixel_art_thumbnail_quality,
            vignette_strength: self.vignette_strength,
        }
    }
}

pub fn app_config() -> &'static AppConfig {
    static CONFIG: OnceLock<AppConfig> = OnceLock::new();
    CONFIG.get_or_init(AppConfig::from_env)
}

fn env_u32(name: &str, default: u32) -> u32 {
    env::var(name)
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(default)
}

fn env_u8(name: &str, default: u8) -> u8 {
    env::var(name)
        .ok()
        .and_then(|v| v.parse::<u8>().ok())
        .unwrap_or(default)
}

fn env_f32(name: &str, default: f32) -> f32 {
    env::var(name)
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .unwrap_or(default)
}

fn env_bool(name: &str, default: bool) -> bool {
    env::var(name)
        .ok()
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(default)
}
