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

fn default_mode_annotation() -> String {
    "Control-a".to_string()
}

fn default_mode_capture() -> String {
    "Control-c".to_string()
}

/// Default screen corner for the OSD legend.
fn default_osd_corner() -> crate::osd::Corner {
    crate::osd::Corner::TopLeft
}

/// Default screen corner for the minimap.
fn default_minimap_corner() -> crate::osd::Corner {
    crate::osd::Corner::BottomRight
}

/// Animation scheme for the minimap outline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum MinimapOutlineScheme {
    /// Continuously cycling RGB gradient.
    Gradient,
    /// A 45-degree angle gradient that slides along the outline over time
    /// (the default).
    #[default]
    AngularGradient,
    /// Segmented dashes that travel around the outline; speed scales
    /// inversely with zoom (further in = slower).
    MarchingAnts,
}

impl MinimapOutlineScheme {
    pub fn name(&self) -> &'static str {
        match self {
            MinimapOutlineScheme::Gradient => "gradient",
            MinimapOutlineScheme::AngularGradient => "angular gradient",
            MinimapOutlineScheme::MarchingAnts => "marching ants",
        }
    }
}

/// Default outline animation speed (1.0 = full speed).
fn default_outline_speed() -> f64 {
    1.0
}

/// Default outline thickness in whole pixels.
fn default_outline_thickness() -> u32 {
    3
}

/// Default zoom-based thickness scaling factor (0.25 = 25 % thicker at max
/// zoom relative to the base thickness).
fn default_outline_zoom_scale() -> f64 {
    0.25
}

/// Default slow-down factor when Shift is held during pointer motion
/// (0.1 = 10 % speed, i.e. 10× slower than normal).
fn default_shift_slow_factor() -> f64 {
    0.1
}

/// Whether to show a small on-screen indicator while Shift slow-down is
/// active (default true).
fn default_show_shift_osd() -> bool {
    true
}

/// Default key that toggles the magnified cursor inside the viewport.
fn default_toggle_cursor() -> String {
    "c".to_string()
}

/// Default key held to smooth-zoom with vertical mouse motion: the middle
/// mouse button ("MMB"). A keyboard modifier (e.g. "Super") can be set
/// instead; the magnifier arms hold-to-zoom on a key press for keyboard
/// bindings and on MMB press for "MMB".
fn default_hold_to_zoom() -> String {
    "MMB".to_string()
}

/// Default key that toggles the minimap overlay (a dimmed overview of the
/// frozen screen with the visible-region marker) in the viewport corner.
fn default_minimap() -> String {
    "m".to_string()
}

/// Default for whether the minimap overlay is visible at launch.
fn default_minimap_visible() -> bool {
    true
}

/// Default for whether panning is locked to the capture's pixel grid (the
/// cursor's texels and the screen's texels stay flush, at the cost of
/// whole-texel pan steps).
fn default_pixel_locked_panning() -> bool {
    true
}

/// Default pan-tuning exponent: the pan distance per mouse pixel is scaled
/// by `zoom^-tuning`, so at high zoom you move the mouse further to pan
/// from one magnified pixel to the next (and below 1× a short nudge travels
/// further). `0` (the default) disables the scaling: a slower-than-hand pan
/// makes the far wall unreachable in a single sweep (the mouse hits the
/// physical edge before the view catches up), so it is opt-in rather than
/// on by default.
fn default_pan_tuning() -> f64 {
    0.0
}

/// Default key that toggles the effective screenshot scale (real size vs
/// magnified) while in Screenshot Mode.
fn default_screenshot_scale_toggle() -> String {
    "v".to_string()
}

/// Default zoom-per-pixel rate for hold-to-zoom vertical motion.
fn default_hold_to_zoom_speed() -> f64 {
    0.02
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct MagnifierConfig {
    pub default_zoom: Option<f64>,
    /// The zoom-key levels (1–9) and the scroll-wheel levels span 1×..=this.
    /// Each numeric key selects `max_zoom * key / 9`, so key 9 is the maximum.
    #[serde(default = "default_max_zoom")]
    pub max_zoom: f64,
    /// How fast hold-to-zoom changes the zoom per pixel of vertical pointer
    /// motion (default 0.02 → 2 % per pixel, i.e. the full 1×–9× range in
    /// 400 px of movement).
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
    /// Screen corner where the OSD legend is placed (default "top-left").
    /// With the centered cursor scheme the OSD no longer relocates
    /// dynamically — it stays in the configured corner.
    #[serde(default = "default_osd_corner")]
    pub osd_corner: crate::osd::Corner,
    /// Screen corner where the minimap overview is placed (default
    /// "bottom-right"). If the minimap's corner coincides with the OSD's
    /// corner, the minimap is automatically offset to avoid overlap.
    #[serde(default = "default_minimap_corner")]
    pub minimap_corner: crate::osd::Corner,
    /// Whether the minimap overlay is visible at launch (default true; the
    /// `minimap` key still toggles it at runtime).
    #[serde(default = "default_minimap_visible")]
    pub minimap_visible: bool,
    /// Animation scheme for the minimap outline border.
    #[serde(default)]
    pub minimap_outline_scheme: MinimapOutlineScheme,
    /// Outline animation speed multiplier (default 0.2, i.e. 5× slower than
    /// the original built-in rate). Higher values speed up the animation.
    #[serde(default = "default_outline_speed")]
    pub minimap_outline_speed: f64,
    /// Outline thickness in whole pixels (default 3).
    #[serde(default = "default_outline_thickness")]
    pub minimap_outline_thickness: u32,
    /// Fractional extra thickness applied at max zoom (default 0.25 = 25 %
    /// thicker at full zoom-in). The effective thickness scales linearly
    /// from the base value at 1× to base × (1 + zoom_scale) at max_zoom.
    #[serde(default = "default_outline_zoom_scale")]
    pub minimap_outline_zoom_scale: f64,
    /// Slow-down factor applied to pointer motion while Shift is held
    /// (default 0.25 = quarter speed).
    #[serde(default = "default_shift_slow_factor")]
    pub shift_slow_factor: f64,
    /// Whether to show a small on-screen indicator while Shift slow-down
    /// is active (default true).
    #[serde(default = "default_show_shift_osd")]
    pub show_shift_osd: bool,
    /// Whether the view center is locked to the capture's pixel grid
    /// (default true): the magnified cursor's texels and the screen's texels
    /// stay flush at every zoom, with panning moving in whole magnified
    /// blocks (one capture pixel per step). When off, panning is smooth and
    /// continuous but the screen's block phase drifts relative to the fixed
    /// cursor, so the blocks can be offset by up to one block width.
    #[serde(default = "default_pixel_locked_panning")]
    pub pixel_locked_panning: bool,
    /// Pan-tuning exponent (default 0.5, range 0..=1): the pan distance per
    /// mouse pixel is scaled by `zoom^-tuning`, so the more you zoom in the
    /// more mouse travel is needed to pan from one magnified pixel to the
    /// next, and below 1× a short nudge travels further ("vice versa"). `0`
    /// disables the scaling (constant pan speed). The view intentionally
    /// lags the hand's content while this is active; pushing into a screen
    /// edge glides the view to the wall so the exact edges stay reachable.
    #[serde(default = "default_pan_tuning")]
    pub pan_tuning: f64,
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
    #[serde(default = "default_mode_annotation")]
    pub mode_annotation: String,
    #[serde(default = "default_mode_capture")]
    pub mode_capture: String,
    /// Reset the zoom back to `default_zoom`. Defaults to "r" for config
    /// files written before it existed. (When the hold-to-zoom binding is
    /// "MMB", the middle mouse button arms hold-to-zoom instead of resetting;
    /// with any other hold-to-zoom binding MMB still resets.)
    #[serde(default = "default_reset_zoom")]
    pub reset_zoom: String,
    /// Toggle the magnified cursor inside the viewport. Defaults to "c"
    /// (config files written before it existed get "c" too, since C was
    /// freed up when the config window moved to Tab).
    #[serde(default = "default_toggle_cursor")]
    pub toggle_cursor: String,
    /// Key held to smooth-zoom with vertical pointer motion. Defaults to
    /// "MMB" (the middle mouse button); a keyboard modifier such as "Super"
    /// (either side) can be set instead.
    #[serde(default = "default_hold_to_zoom")]
    pub hold_to_zoom: String,
    /// Toggle the effective screenshot scale (real vs magnified) while in
    /// Screenshot Mode. Defaults to "v".
    #[serde(default = "default_screenshot_scale_toggle")]
    pub screenshot_scale_toggle: String,
    /// Toggle the minimap overlay (a dimmed overview of the frozen screen
    /// with a marker for the visible region / cursor position). Defaults to
    /// "m" for config files written before it existed.
    #[serde(default = "default_minimap")]
    pub minimap: String,
}

impl Default for Keybindings {
    fn default() -> Self {
        MagnifierConfig::default().keybindings
    }
}

impl Default for MagnifierConfig {
    fn default() -> Self {
        MagnifierConfig {
            default_zoom: Some(1.0),
            max_zoom: 9.0,
            hold_to_zoom_speed: 0.02,
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
                mode_annotation: "Control-a".to_string(),
                mode_capture: "Control-c".to_string(),
                reset_zoom: "r".to_string(),
                toggle_cursor: "c".to_string(),
                hold_to_zoom: "MMB".to_string(),
                screenshot_scale_toggle: "v".to_string(),
                minimap: "m".to_string(),
            },
            screenshot_scale: ScreenshotScale::Real,
            screenshot_path: "~/Pictures".to_string(),
            screenshot_filename_pattern: "maggie_%Y%m%d_%H%M%S.png".to_string(),
            screenshot_selection_color: [255, 153, 0],
            show_osd: false,
            osd_corner: crate::osd::Corner::TopLeft,
            minimap_corner: crate::osd::Corner::BottomRight,
            minimap_visible: true,
            minimap_outline_scheme: MinimapOutlineScheme::AngularGradient,
            minimap_outline_speed: 1.0,
            minimap_outline_thickness: 3,
            minimap_outline_zoom_scale: 0.25,
            shift_slow_factor: 0.1,
            show_shift_osd: true,
            pixel_locked_panning: true,
            pan_tuning: 0.0,
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
            0.02
        };
    config.pan_tuning = if config.pan_tuning.is_finite() {
        config.pan_tuning.clamp(0.0, 1.0)
    } else {
        0.0
    };
    config.minimap_outline_speed = if config.minimap_outline_speed.is_finite() {
        config.minimap_outline_speed.clamp(0.01, 10.0)
    } else {
        1.0
    };
    config.minimap_outline_thickness = config.minimap_outline_thickness.clamp(1, 8);
    config.minimap_outline_zoom_scale = if config.minimap_outline_zoom_scale.is_finite() {
        config.minimap_outline_zoom_scale.clamp(0.0, 2.0)
    } else {
        0.25
    };
    config.shift_slow_factor = if config.shift_slow_factor.is_finite() {
        config.shift_slow_factor.clamp(0.01, 1.0)
    } else {
        0.1
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
    fn minimap_default_is_m() {
        // The minimap toggle default is "m"; config files written before the
        // key existed get it via the serde default (the struct-update below
        // exercises the same fill-in path the migration tests rely on).
        assert_eq!(MagnifierConfig::default().keybindings.minimap, "m");
        let keybindings = Keybindings {
            ..Keybindings::default()
        };
        assert_eq!(keybindings.minimap, "m");
    }

    #[test]
    fn minimap_visible_defaults_to_true() {
        // The minimap shows by default; config files written before the field
        // existed get it via the serde default, and the `minimap` key still
        // toggles it at runtime.
        assert!(MagnifierConfig::default().minimap_visible);
        let config = MagnifierConfig {
            ..MagnifierConfig::default()
        };
        assert!(config.minimap_visible);
    }

    #[test]
    fn pixel_locked_panning_defaults_to_true() {
        // Panning locks to the capture pixel grid by default (flush texels,
        // whole-block steps); config files written before the field existed
        // get it via the serde default.
        assert!(MagnifierConfig::default().pixel_locked_panning);
        let config = MagnifierConfig {
            ..MagnifierConfig::default()
        };
        assert!(config.pixel_locked_panning);
    }

    #[test]
    fn pan_tuning_defaults_to_off() {
        // Pan-tuning is off by default (0); config files written before the
        // field existed get it via the serde default, and normalization
        // clamps out-of-range values.
        assert_eq!(MagnifierConfig::default().pan_tuning, 0.0);
        let mut config = MagnifierConfig {
            ..MagnifierConfig::default()
        };
        config.pan_tuning = 5.0;
        normalize_config(&mut config);
        assert_eq!(config.pan_tuning, 1.0);
        config.pan_tuning = f64::NAN;
        normalize_config(&mut config);
        assert_eq!(config.pan_tuning, 0.0);
    }

    #[test]
    fn hold_to_zoom_default_is_mmb() {
        // The hold-to-zoom default is MMB (the middle mouse button); an
        // explicit legacy "Space" binding is left exactly as the user wrote
        // it.
        let mut config = MagnifierConfig {
            keybindings: Keybindings {
                hold_to_zoom: "Space".to_string(),
                ..Keybindings::default()
            },
            ..MagnifierConfig::default()
        };
        normalize_config(&mut config);
        assert_eq!(config.keybindings.hold_to_zoom, "Space");
        // And the default is MMB.
        assert_eq!(MagnifierConfig::default().keybindings.hold_to_zoom, "MMB");
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
        assert_eq!(config.hold_to_zoom_speed, 0.02);
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
