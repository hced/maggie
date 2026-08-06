#![allow(dead_code)]

use serde::{Deserialize, Serialize};

/// How the scroll wheel changes the zoom.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ScrollZoomMode {
    /// Each wheel notch steps to the next zoom level of the `1`–`9` keys
    /// (default).
    #[default]
    Levels,
    /// Each wheel notch multiplies the zoom factor by a fixed step (10 %).
    Factor,
}

/// Default maximum zoom when a config file has no `max_zoom` (keeps the
/// historical 1–9 zoom-key behavior for unconfigured installs).
fn default_max_zoom() -> f64 {
    9.0
}

/// Default "reset zoom" keybinding for config files written before the key
/// existed (serde fills it in when the field is missing).
fn default_reset_zoom() -> String {
    "r".to_string()
}

/// Default key that toggles the magnified cursor inside the viewport.
fn default_toggle_cursor() -> String {
    "c".to_string()
}

/// Default modifier key held to smooth-zoom with vertical mouse motion.
/// (Changed from "Super" to "Space" at the user's request.)
fn default_hold_to_zoom() -> String {
    "Space".to_string()
}

/// Default key that toggles the effective screenshot scale (real size vs
/// magnified) while in Screenshot Mode.
fn default_screenshot_scale_toggle() -> String {
    "v".to_string()
}

/// Default zoom-per-pixel rate for hold-to-zoom vertical motion.
fn default_hold_to_zoom_speed() -> f64 {
    0.05
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct MagnifierConfig {
    pub default_zoom: Option<f64>,
    /// The zoom-key levels (1–9) and the scroll-wheel levels span 1×..=this.
    /// Each numeric key selects `max_zoom * key / 9`, so key 9 is the maximum.
    #[serde(default = "default_max_zoom")]
    pub max_zoom: f64,
    /// How fast hold-to-zoom changes the zoom per pixel of vertical pointer
    /// motion (default 0.05 → 5 % per pixel, i.e. the full 1×–9× range in
    /// 160 px of movement).
    #[serde(default = "default_hold_to_zoom_speed")]
    pub hold_to_zoom_speed: f64,
    #[serde(default)]
    pub scroll_zoom_mode: ScrollZoomMode,
    #[serde(default)]
    pub invert_scroll_zoom: bool,
    /// Whether all zoom operations (scroll wheel, hold-to-zoom and the `1`–`9`
    /// preset keys) may zoom out below 1×, down to the **fully-zoomed-out
    /// view** — the whole captured screen filling the viewport, referred to in
    /// the UI as "0 %". When disabled the minimum zoom is 1×. Forced on while
    /// `default_zoom` is exactly 0 (see [`MagnifierConfig::min_zoom`]). The
    /// `0` key always reaches the fully-zoomed-out view regardless of this
    /// setting.
    #[serde(default)]
    pub allow_zero_zoom: bool,
    pub keybindings: Keybindings,
    pub screenshot_path: String,
    pub screenshot_filename_pattern: String,
    /// RGB color of the selection rectangle border shown in Screenshot Mode
    /// (manual/fullscreen selection). Default: orange.
    #[serde(default = "default_screenshot_selection_color")]
    pub screenshot_selection_color: [u8; 3],
    /// Whether saved screenshots are the real pixels of the selection or the
    /// selection magnified to the current zoom level.
    #[serde(default)]
    pub screenshot_scale: ScreenshotScale,
    #[serde(default)]
    pub show_osd: bool,
}

/// What the saved screenshot represents: the real pixels of the selected
/// region, or the same region scaled up to the current magnifier zoom.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ScreenshotScale {
    /// Real size: 1 saved pixel = 1 captured pixel (default).
    #[default]
    Real,
    /// The selected region magnified to the current zoom level (nearest
    /// neighbor, matching the crisp magnifier look). At zoom below 1×
    /// (the fully-zoomed-out view) the saved image is clamped to real size.
    Magnified,
}

impl ScreenshotScale {
    /// Human-readable label, shared by the Configuration dropdown and the
    /// Screenshot-Mode Key Legend.
    pub fn name(&self) -> &'static str {
        match self {
            ScreenshotScale::Real => "real size",
            ScreenshotScale::Magnified => "magnified",
        }
    }
}

/// Default screenshot selection border color (orange).
fn default_screenshot_selection_color() -> [u8; 3] {
    [255, 153, 0]
}

impl MagnifierConfig {
    /// The effective minimum zoom for all zoom operations, as a factor:
    /// **0** when 0 % zoom is allowed (`allow_zero_zoom`) — or forced by a
    /// default zoom of exactly 0 — otherwise **1×**. The engine maps the 0 to
    /// the fully-zoomed-out view (the whole captured screen filling the
    /// viewport) at runtime; the `0` key always reaches that view regardless
    /// of this, per the spec.
    pub fn min_zoom(&self) -> f64 {
        if self.allow_zero_zoom || self.default_zoom == Some(0.0) {
            0.0
        } else {
            1.0
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
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
    /// Reset the zoom back to `default_zoom` (the middle mouse button does
    /// the same). Defaults to "r" for config files written before it existed.
    #[serde(default = "default_reset_zoom")]
    pub reset_zoom: String,
    /// Toggle the magnified cursor inside the viewport. Defaults to "c"
    /// (config files written before it existed get "c" too, since C was
    /// freed up when the config window moved to Tab).
    #[serde(default = "default_toggle_cursor")]
    pub toggle_cursor: String,
    /// Modifier key held to smooth-zoom with vertical pointer motion. Defaults
    /// to "Super" (the Super/Mod key, either side).
    #[serde(default = "default_hold_to_zoom")]
    pub hold_to_zoom: String,
    /// Toggle the effective screenshot scale (real vs magnified) while in
    /// Screenshot Mode. Defaults to "v".
    #[serde(default = "default_screenshot_scale_toggle")]
    pub screenshot_scale_toggle: String,
}

impl Default for Keybindings {
    fn default() -> Self {
        MagnifierConfig::default().keybindings
    }
}

impl Default for MagnifierConfig {
    fn default() -> Self {
        MagnifierConfig {
            default_zoom: Some(3.0),
            max_zoom: 9.0,
            hold_to_zoom_speed: 0.05,
            scroll_zoom_mode: ScrollZoomMode::Levels,
            invert_scroll_zoom: false,
            allow_zero_zoom: false,
            keybindings: Keybindings {
                toggle_osd: "k".to_string(),
                screenshot_manual: "g".to_string(),
                screenshot_window: "w".to_string(),
                screenshot_fullscreen: "f".to_string(),
                config_window: "Tab".to_string(),
                anti_aliasing: "a".to_string(),
                mode_center_cursor: "Control-c".to_string(),
                mode_edge_pan: "Control-e".to_string(),
                mode_miniature: "Control-m".to_string(),
                reset_zoom: "r".to_string(),
                toggle_cursor: "c".to_string(),
                hold_to_zoom: "Space".to_string(),
                screenshot_scale_toggle: "v".to_string(),
            },
            screenshot_scale: ScreenshotScale::Real,
            screenshot_path: "~/Pictures".to_string(),
            screenshot_filename_pattern: "maggie_%Y%m%d_%H%M%S.png".to_string(),
            screenshot_selection_color: [255, 153, 0],
            show_osd: true,
        }
    }
}

pub fn load_config() -> anyhow::Result<MagnifierConfig> {
    let config_dir = dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not find config directory"))?
        .join("maggie");

    let config_file = config_dir.join("config.ron");
    let mut config = if config_file.exists() {
        let contents = std::fs::read_to_string(config_file)?;
        ron::from_str(&contents).map_err(|e| anyhow::anyhow!("Config parse error: {}", e))?
    } else {
        MagnifierConfig::default()
    };
    normalize_config(&mut config);
    Ok(config)
}

/// Clamp/normalize a loaded config so hand-edited or out-of-range values can't
/// poison runtime behavior (NaN in an `f64` field would make the auto-save
/// drift check — `NaN != NaN` is always true — rewrite the file every frame).
/// Also runs the keybinding migrations for renamed defaults.
pub(crate) fn normalize_config(config: &mut MagnifierConfig) {
    const MIN_ZOOM: f64 = 1.0;
    const MAX_ZOOM: f64 = 32.0;
    config.max_zoom = if config.max_zoom.is_finite() {
        config.max_zoom.clamp(MIN_ZOOM, MAX_ZOOM)
    } else {
        9.0
    };
    config.hold_to_zoom_speed =
        if config.hold_to_zoom_speed.is_finite() && config.hold_to_zoom_speed > 0.0 {
            config.hold_to_zoom_speed.clamp(0.001, 1.0)
        } else {
            0.05
        };
    // A default zoom of exactly 0 % requires 0 % zoom to be reachable, so the
    // allow-zero setting is forced on while it stays at 0 (the Configuration
    // window mirrors this and locks the checkbox).
    if config.default_zoom == Some(0.0) {
        config.allow_zero_zoom = true;
    }
    let min_zoom = config.min_zoom();
    config.default_zoom = config
        .default_zoom
        .filter(|v| v.is_finite())
        .map(|v| v.clamp(min_zoom, config.max_zoom));
    // Migration: the Configuration window's default key moved from `C` to
    // `Tab`, and `C` became the magnified-cursor toggle. A config file written
    // before `toggle_cursor` existed has `config_window: "c"` and gets
    // `toggle_cursor` defaulted to "c" — which would collide (the config
    // window is matched first, so the cursor toggle would be unreachable and
    // Tab would do nothing). Detect that exact collision and move the config
    // window to the new default.
    if config.keybindings.config_window == "c" && config.keybindings.toggle_cursor == "c" {
        config.keybindings.config_window = "Tab".to_string();
    }
    // Migration: the hold-to-zoom default moved from Super to Space. Config
    // files saved before the change carry "Super"; migrate them to the new
    // default the user asked for.
    if config.keybindings.hold_to_zoom == "Super" {
        config.keybindings.hold_to_zoom = "Space".to_string();
    }
    // Migration: the manual-screenshot (Screenshot Mode) key moved from `S`
    // to `G` so the WASD keys are owned by selection-border nudging (S
    // collided with the nudge-down key). Config files carrying the old
    // default migrate; other explicit bindings are left alone.
    if config.keybindings.screenshot_manual == "s" {
        config.keybindings.screenshot_manual = "g".to_string();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pre_cursor_toggle_configs_migrate_config_window_to_tab() {
        // A config as written before `toggle_cursor` existed: config window on
        // "c", toggle_cursor serde-defaulted to "c". Must migrate so C is the
        // cursor toggle and Tab opens the window.
        let mut config = MagnifierConfig {
            keybindings: Keybindings {
                config_window: "c".to_string(),
                toggle_cursor: "c".to_string(),
                ..Keybindings::default()
            },
            ..MagnifierConfig::default()
        };
        normalize_config(&mut config);
        assert_eq!(config.keybindings.config_window, "Tab");
        assert_eq!(config.keybindings.toggle_cursor, "c");
    }

    #[test]
    fn pre_space_configs_migrate_hold_to_zoom() {
        // Configs saved while the hold-to-zoom default was Super migrate to
        // the new Space default.
        let mut config = MagnifierConfig {
            keybindings: Keybindings {
                hold_to_zoom: "Super".to_string(),
                ..Keybindings::default()
            },
            ..MagnifierConfig::default()
        };
        normalize_config(&mut config);
        assert_eq!(config.keybindings.hold_to_zoom, "Space");
        // The new default also is Space.
        assert_eq!(MagnifierConfig::default().keybindings.hold_to_zoom, "Space");
    }

    #[test]
    fn normalize_config_clamps_zooms_and_speed() {
        let mut config = MagnifierConfig {
            default_zoom: Some(f64::NAN),
            max_zoom: 99.0,
            hold_to_zoom_speed: f64::NEG_INFINITY,
            ..MagnifierConfig::default()
        };
        normalize_config(&mut config);
        assert_eq!(config.max_zoom, 32.0);
        assert_eq!(config.default_zoom, None); // NaN dropped
        assert_eq!(config.hold_to_zoom_speed, 0.05);
    }

    #[test]
    fn default_zoom_zero_forces_allow_zero_zoom() {
        // A default zoom of 0 % forces the allow-zero setting on, so the app
        // can actually launch at the configured 0 % zoom.
        let mut config = MagnifierConfig {
            default_zoom: Some(0.0),
            ..MagnifierConfig::default()
        };
        normalize_config(&mut config);
        assert!(config.allow_zero_zoom);
        assert_eq!(config.min_zoom(), 0.0);
        assert_eq!(config.default_zoom, Some(0.0));
    }

    #[test]
    fn default_zoom_above_zero_leaves_allow_zero_free() {
        // With a default zoom above 0 the setting stays exactly as written.
        let mut config = MagnifierConfig {
            default_zoom: Some(3.0),
            allow_zero_zoom: false,
            ..MagnifierConfig::default()
        };
        normalize_config(&mut config);
        assert!(!config.allow_zero_zoom);
        assert_eq!(config.min_zoom(), 1.0);
    }

    #[test]
    fn default_zoom_with_allow_zero_enabled_can_go_below_1x() {
        // When 0 % zoom is allowed, a sub-1x default (e.g. 0.5) is kept
        // instead of being clamped to 1x.
        let mut config = MagnifierConfig {
            default_zoom: Some(0.5),
            allow_zero_zoom: true,
            ..MagnifierConfig::default()
        };
        normalize_config(&mut config);
        assert_eq!(config.default_zoom, Some(0.5));
        assert_eq!(config.min_zoom(), 0.0);
    }

    #[test]
    fn old_s_manual_screenshot_key_migrates_to_g() {
        // The manual-screenshot key moved from S to G (so the WASD keys are
        // owned by border nudging in Screenshot Mode); configs carrying the
        // old default migrate on load, while a different explicit binding is
        // left alone.
        let mut old = MagnifierConfig {
            keybindings: Keybindings {
                screenshot_manual: "s".to_string(),
                ..Keybindings::default()
            },
            ..MagnifierConfig::default()
        };
        normalize_config(&mut old);
        assert_eq!(old.keybindings.screenshot_manual, "g");

        let mut custom = MagnifierConfig {
            keybindings: Keybindings {
                screenshot_manual: "x".to_string(),
                ..Keybindings::default()
            },
            ..MagnifierConfig::default()
        };
        normalize_config(&mut custom);
        assert_eq!(custom.keybindings.screenshot_manual, "x");
        // And the new default is G.
        assert_eq!(
            MagnifierConfig::default().keybindings.screenshot_manual,
            "g"
        );
    }

    #[test]
    fn config_with_removed_edge_fill_field_still_loads() {
        // Configs written while the `htz_edge_behavior` / `htz_edge_fill`
        // options existed still load (ron ignores unknown fields).
        let legacy = ron::from_str::<MagnifierConfig>(
            "(htz_edge_behavior: pin, htz_edge_fill: stretch, keybindings: (\n\
             toggle_osd: \"k\", screenshot_manual: \"s\", screenshot_window: \"w\",\n\
             screenshot_fullscreen: \"f\", config_window: \"Tab\", anti_aliasing: \"a\",\n\
             mode_center_cursor: \"Control-c\", mode_edge_pan: \"Control-e\",\n\
             mode_miniature: \"Control-m\", reset_zoom: \"r\", toggle_cursor: \"c\",\n\
             hold_to_zoom: \"Space\"), screenshot_path: \"x\",\n\
             screenshot_filename_pattern: \"maggie_%Y%m%d_%H%M%S.png\")",
        )
        .expect("legacy config with unknown fields loads");
        // Removed options fall back to defaults; the rest is intact.
        assert_eq!(legacy.keybindings.hold_to_zoom, "Space");
        assert_eq!(legacy.keybindings.config_window, "Tab");
        assert_eq!(legacy.screenshot_path, "x");
        // Newer keybindings added since this config was written get defaults.
        assert_eq!(legacy.keybindings.screenshot_scale_toggle, "v");
    }
}
