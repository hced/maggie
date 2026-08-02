#![allow(dead_code)]

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MagnifierConfig {
    pub default_zoom: Option<f64>,
    pub keybindings: Keybindings,
    pub screenshot_path: String,
    pub screenshot_filename_pattern: String,
    pub show_osd: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Keybindings {
    pub toggle_osd: String,
    pub screenshot_manual: String,
    pub screenshot_window: String,
    pub screenshot_fullscreen: String,
    pub config_window: String,
    pub anti_aliasing: String,
    pub mode_center_cursor: String,
    pub mode_edge_pan: String,
    pub mode_miniature: String,
}

impl Default for MagnifierConfig {
    fn default() -> Self {
        MagnifierConfig {
            default_zoom: Some(3.0),
            keybindings: Keybindings {
                toggle_osd: "k".to_string(),
                screenshot_manual: "s".to_string(),
                screenshot_window: "w".to_string(),
                screenshot_fullscreen: "f".to_string(),
                config_window: "c".to_string(),
                anti_aliasing: "a".to_string(),
                mode_center_cursor: "Control-c".to_string(),
                mode_edge_pan: "Control-e".to_string(),
                mode_miniature: "Control-m".to_string(),
            },
            screenshot_path: "~/Pictures".to_string(),
            screenshot_filename_pattern: "maggie_%Y%m%d_%H%M%S.png".to_string(),
            show_osd: false,
        }
    }
}

pub fn load_config() -> anyhow::Result<MagnifierConfig> {
    let config_dir = dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not find config directory"))?
        .join("maggie");

    let config_file = config_dir.join("config.ron");
    if config_file.exists() {
        let contents = std::fs::read_to_string(config_file)?;
        ron::from_str(&contents).map_err(|e| anyhow::anyhow!("Config parse error: {}", e))
    } else {
        Ok(MagnifierConfig::default())
    }
}

pub fn save_config(config: &MagnifierConfig) -> anyhow::Result<()> {
    let config_dir = dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not find config directory"))?
        .join("maggie");

    std::fs::create_dir_all(&config_dir)?;

    let config_file = config_dir.join("config.ron");
    let contents = ron::ser::to_string_pretty(config, ron::ser::PrettyConfig::default())?;
    std::fs::write(config_file, contents)?;

    tracing::info!("Config saved to directory");
    Ok(())
}
