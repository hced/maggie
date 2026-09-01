//! The egui-based **Configuration** window.
//!
//! Maggie is a full-screen Wayland layer-shell overlay, so instead of opening
//! a separate toplevel window the configuration UI is painted directly into
//! the same EGL surface (via `egui-glow` over the existing GLES2 context)
//! while it is open, taking over pointer and keyboard input for the duration.
//!
//! The UI edits the live [`MagnifierConfig`]; "Save" persists it to
//! `~/.config/maggie/config.ron`, "Reload" re-reads it from disk.

use std::sync::Arc;

use egui::{Context, Event, FullOutput, Modifiers, RawInput, ViewportId, ViewportInfo};
use egui_glow::Painter;
use glow::Context as GlowContext;

use crate::config::MagnifierConfig;
use crate::gpu::RENDER_SCALE;

/// What the Configuration window wants the engine to do after a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiResult {
    /// Keep the window open.
    Continue,
    /// The user closed the window; the engine should return to the magnifier.
    Close,
}

pub struct ConfigWindow {
    ctx: Context,
    painter: Painter,
    /// Logical surface size the UI is laid out in (points).
    width: i32,
    height: i32,
    /// Last known pointer position, in logical surface coordinates.
    pointer_pos: egui::Pos2,
    modifiers: Modifiers,
    /// Events queued by the input handlers since the last frame.
    pending: Vec<Event>,
    /// Shapes + textures from the last `update()`, consumed by `paint()`.
    shapes: Vec<egui::epaint::ClippedShape>,
    textures_delta: egui::TexturesDelta,
    pixels_per_point: f32,
    /// Status line shown under the buttons ("saved", errors, ...).
    status: Option<(String, bool)>,
    /// Monotonic clock start: `RawInput.time` is fed from this so egui
    /// animations (caret blink, progress bars) run at real speed. Without a
    /// growing `time` value egui advances its animations as fast as frames
    /// are produced, which makes the text caret blink frantically.
    started: std::time::Instant,
    /// Config as last persisted to disk (or loaded from it). Any drift from
    /// the live config triggers an auto-save.
    saved: MagnifierConfig,
    /// Native folder picker for the screenshot path.
    file_dialog: egui_file_dialog::FileDialog,
}

impl ConfigWindow {
    /// Create the window. The EGL/GLES context must be current on this thread
    /// (the `Painter` compiles shaders immediately). `initial` is the live
    /// config snapshot used as the baseline for auto-saving.
    pub fn new(
        gl: Arc<GlowContext>,
        width: i32,
        height: i32,
        initial: MagnifierConfig,
    ) -> anyhow::Result<Self> {
        let painter = Painter::new(gl, "", None, false)
            .map_err(|e| anyhow::anyhow!("egui-glow painter init failed: {e}"))?;
        let ctx = Context::default();
        // Larger base fonts: the Configuration window is read from a distance
        // (whole-screen surface), and the previous sizes were too small.
        ctx.style_mut_of(egui::Theme::Dark, |style| {
            style.text_styles = [
                (egui::TextStyle::Heading, egui::FontId::proportional(28.0)),
                (egui::TextStyle::Body, egui::FontId::proportional(19.0)),
                (egui::TextStyle::Button, egui::FontId::proportional(19.0)),
                (egui::TextStyle::Small, egui::FontId::proportional(16.0)),
                (egui::TextStyle::Monospace, egui::FontId::monospace(17.0)),
            ]
            .into();
            style.spacing.item_spacing = egui::vec2(10.0, 8.0);
        });
        Ok(Self {
            ctx,
            painter,
            width: width.max(1),
            height: height.max(1),
            pointer_pos: egui::Pos2::ZERO,
            modifiers: Modifiers::NONE,
            pending: Vec::new(),
            shapes: Vec::new(),
            textures_delta: egui::TexturesDelta::default(),
            pixels_per_point: RENDER_SCALE as f32,
            status: None,
            started: std::time::Instant::now(),
            saved: initial, // `as_modal(false)`: the default modal mode draws a full-screen
            // interactive overlay behind the dialog. In a hand-painted egui
            // setup the overlay can end up above the dialog and swallow every
            // click (the dialog is then only reachable via keyboard); a plain
            // (non-modal) egui Window is always above our background panel and
            // stays clickable and draggable.
            file_dialog: egui_file_dialog::FileDialog::new()
                .default_size(egui::vec2(640.0, 440.0))
                .as_modal(false),
        })
    }

    pub fn resize(&mut self, width: i32, height: i32) {
        self.width = width.max(1);
        self.height = height.max(1);
    }

    // ------------------------------------------------------------------
    // Input plumbing (called from the engine's Wayland handlers)
    // ------------------------------------------------------------------

    pub fn pointer_moved(&mut self, pos: egui::Pos2) {
        self.pointer_pos = pos;
        self.pending.push(Event::PointerMoved(pos));
    }

    pub fn pointer_button(&mut self, button: egui::PointerButton, pressed: bool) {
        let pos = self.pointer_pos;
        let modifiers = self.modifiers;
        self.pending.push(Event::PointerButton {
            pos,
            button,
            pressed,
            modifiers,
        });
    }

    pub fn pointer_axis(&mut self, delta: egui::Vec2) {
        let modifiers = self.modifiers;
        self.pending.push(Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta,
            phase: egui::TouchPhase::Move,
            modifiers,
        });
    }

    pub fn pointer_left(&mut self) {
        self.pending.push(Event::PointerGone);
    }

    /// A key press/release. `repeat` only applies to presses.
    pub fn key(&mut self, key: egui::Key, pressed: bool, repeat: bool) {
        let modifiers = self.modifiers;
        self.pending.push(Event::Key {
            key,
            physical_key: None,
            pressed,
            repeat,
            modifiers,
        });
    }

    pub fn text(&mut self, text: String) {
        if !text.chars().any(|c| !c.is_control()) {
            return;
        }
        self.pending.push(Event::Text(text));
    }

    pub fn set_modifiers(&mut self, m: Modifiers) {
        self.modifiers = m;
    }

    // ------------------------------------------------------------------
    // Frame
    // ------------------------------------------------------------------

    /// Run one egui frame, letting the user edit `config`.
    pub fn update(&mut self, config: &mut MagnifierConfig) -> UiResult {
        let raw = RawInput {
            viewport_id: ViewportId::ROOT,
            viewports: ViewportIdMap::from_iter([(
                ViewportId::ROOT,
                ViewportInfo {
                    native_pixels_per_point: Some(RENDER_SCALE as f32),
                    ..Default::default()
                },
            )]),
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(self.width as f32, self.height as f32),
            )),
            modifiers: self.modifiers,
            events: std::mem::take(&mut self.pending),
            focused: true,
            time: Some(self.started.elapsed().as_secs_f64()),
            ..Default::default()
        };

        let mut action = UiResult::Continue;
        let status = self.status.take();

        // The UI closure borrows only locals + `config`, never `self`, so it
        // can run while `self.ctx` is borrowed by `run_ui`.
        let mut new_status: Option<(String, bool)> = None;
        let mut saved_snapshot = false;
        let mut reloaded = false;

        let ctx = &self.ctx;
        let file_dialog = &mut self.file_dialog;
        let full: FullOutput = ctx.run_ui(raw, |ui| {
            egui::CentralPanel::default()
                .frame(
                    egui::Frame::NONE
                        .fill(egui::Color32::from_rgb(0x13, 0x15, 0x1d))
                        .inner_margin(egui::Margin::symmetric(24, 24)),
                )
                .show(ui, |ui| {
                    ui.add_space(4.0);
                    ui.heading("Maggie Configuration");
                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(8.0);

                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.push_id("config_form", |ui| {
                                config_section(ui, config, file_dialog);
                            });
                        });

                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(8.0);

                    ui.horizontal(|ui| {
                        if ui.button("Save").clicked() {
                            match crate::config::save_config(config) {
                                Ok(()) => {
                                    saved_snapshot = true;
                                    new_status = Some((
                                        "Configuration saved to ~/.config/maggie/config.ron".into(),
                                        false,
                                    ))
                                }
                                Err(e) => {
                                    new_status = Some((format!("Save failed: {e:#}"), true));
                                }
                            }
                        }
                        if ui.button("Reload").clicked() {
                            match crate::config::load_config() {
                                Ok(loaded) => {
                                    *config = loaded;
                                    reloaded = true;
                                    new_status =
                                        Some(("Configuration reloaded from disk".into(), false));
                                }
                                Err(e) => {
                                    new_status = Some((format!("Reload failed: {e:#}"), true));
                                }
                            }
                        }
                        if ui.button("Close").clicked() {
                            action = UiResult::Close;
                        }
                    });

                    if let Some((message, is_error)) = status.as_ref() {
                        let color = if *is_error {
                            egui::Color32::from_rgb(0xe0, 0x6a, 0x6a)
                        } else {
                            egui::Color32::from_rgb(0x8f, 0xc8, 0x8f)
                        };
                        ui.add_space(6.0);
                        ui.colored_label(color, message);
                    }

                    if ui.ctx().input(|i| i.key_pressed(egui::Key::Escape)) {
                        action = UiResult::Close;
                    }
                });

            // The native folder picker must be added **inside the same egui
            // pass** as the rest of the UI. egui resolves widget interactions
            // (hit-testing, clicks) against the pass's input state; the
            // dialog used to be updated *after* the pass ended, so its
            // window was rendered a frame late and its widgets' clicks were
            // evaluated against stale input — only scroll and keyboard (which
            // bypass widget-level hit tests) reached it. Inside the pass it
            // behaves exactly like the rest of the Configuration UI.
            file_dialog.update(ctx);
        });

        if let Some(s) = new_status {
            self.status = Some(s);
        }
        // Apply a picked directory to the screenshot path, so the auto-save
        // below persists it on this very frame.
        if let Some(path) = self.file_dialog.take_picked() {
            config.screenshot_path = path.to_string_lossy().into_owned();
        }

        // Keep the persisted snapshot in sync after an explicit Save/Reload,
        // so the drift check below does not re-save redundantly.
        if saved_snapshot || reloaded {
            self.saved = config.clone();
        }
        // Auto-save: any change to the live config (a modified field, a folder
        // picked via the file dialog, ...) is written to disk immediately, so
        // settings survive restarts even without clicking Save.
        if *config != self.saved {
            match crate::config::save_config(config) {
                Ok(()) => {
                    self.saved = config.clone();
                    self.status = Some((
                        "Changes auto-saved to ~/.config/maggie/config.ron".into(),
                        false,
                    ));
                }
                Err(e) => self.status = Some((format!("Auto-save failed: {e:#}"), true)),
            }
        }

        self.shapes = full.shapes;
        self.textures_delta = full.textures_delta;
        self.pixels_per_point = full.pixels_per_point;
        action
    }

    /// Paint the result of the last [`Self::update`] into the current GL
    /// framebuffer. Callers are responsible for presenting afterwards.
    pub fn paint(&mut self) {
        let shapes = std::mem::take(&mut self.shapes);
        let textures_delta = std::mem::take(&mut self.textures_delta);
        let clipped = self.ctx.tessellate(shapes, self.pixels_per_point);
        let screen_px = [
            (self.width as u32) * (RENDER_SCALE as u32),
            (self.height as u32) * (RENDER_SCALE as u32),
        ];
        self.painter.paint_and_update_textures(
            screen_px,
            self.pixels_per_point,
            &clipped,
            &textures_delta,
        );
    }

    pub fn destroy(&mut self) {
        self.painter.destroy();
    }
}

use egui::ViewportIdMap;

// ----------------------------------------------------------------------
// The settings form
// ----------------------------------------------------------------------

fn config_section(
    ui: &mut egui::Ui,
    config: &mut MagnifierConfig,
    file_dialog: &mut egui_file_dialog::FileDialog,
) {
    egui::Grid::new("settings_grid")
        .num_columns(2)
        .spacing([16.0, 10.0])
        .show(ui, |ui| {
            ui.label("Default zoom");
            // `get_or_insert` edits the real stored value (the previous
            // `unwrap_or` edited a temporary, so the setting never changed).
            // The range starts at 0: dragging it to exactly 0 % auto-enables
            // the "Allow 0% zoom" option below (and locks it while it stays
            // at 0).
            ui.add(
                egui::DragValue::new(config.default_zoom.get_or_insert(3.0))
                    .range(0.0..=32.0)
                    .speed(0.1),
            );
            ui.end_row();

            ui.label("Max zoom");
            // The parenthetical lives in the same second-column cell as the
            // widget (wrapped in a horizontal) so it lines up with the other
            // parentheticals instead of spilling onto a stray row.
            ui.horizontal(|ui| {
                ui.add(
                    egui::DragValue::new(&mut config.max_zoom)
                        .range(1.0..=32.0)
                        .speed(0.1),
                );
                ui.label("(0-9 keys and the wheel span up to this; key 9 = max)");
            });
            ui.end_row();

            ui.label("Allow 0% zoom");
            // While the default zoom is exactly 0 %, 0 % zoom is required (the
            // app would otherwise be unable to launch at the configured
            // default) — the option is forced on and cannot be disabled. The
            // warning disappears as soon as the default zoom is above 0.
            let zero_forced = config.default_zoom == Some(0.0);
            if zero_forced {
                config.allow_zero_zoom = true;
            }
            let mut allow_zero = config.allow_zero_zoom;
            ui.horizontal(|ui| {
                ui.add_enabled(!zero_forced, egui::Checkbox::new(&mut allow_zero, ""));
                if zero_forced {
                    ui.colored_label(
                        egui::Color32::from_rgb(0xe0, 0xb0, 0x50),
                        "locked: default zoom is 0% (0% zoom is required while it stays at 0)",
                    );
                } else {
                    ui.label("(enables wheel and hold-to-zoom to reach 0%; the 0 key always zooms to 0%)");
                }
            });
            ui.end_row();
            if allow_zero != config.allow_zero_zoom {
                config.allow_zero_zoom = allow_zero;
            }

            ui.label("Scroll zoom mode");
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("scroll_zoom_mode")
                    .selected_text(scroll_zoom_name(config.scroll_zoom_mode))
                    .show_ui(ui, |ui| {
                        for mode in [
                            crate::config::ScrollZoomMode::Levels,
                            crate::config::ScrollZoomMode::Factor,
                        ] {
                            ui.selectable_value(
                                &mut config.scroll_zoom_mode,
                                mode,
                                scroll_zoom_name(mode),
                            );
                        }
                    });
                ui.label("(levels = 1-9 keys, factor = 10% steps)");
            });
            ui.end_row();

            ui.label("Invert scroll zoom");
            ui.checkbox(&mut config.invert_scroll_zoom, "");
            ui.end_row();

            ui.label("Show OSD legend at start");
            ui.checkbox(&mut config.show_osd, "");
            ui.end_row();

            ui.label("Show minimap at start");
            ui.checkbox(&mut config.minimap_visible, "");
            ui.end_row();

            ui.label("Pixel-locked panning");
            ui.horizontal(|ui| {
                ui.checkbox(&mut config.pixel_locked_panning, "");
                ui.label("(cursor & screen texels stay flush; pans in whole magnified blocks — off for smooth panning)");
            });
            ui.end_row();

            ui.label("Pan tuning");
            ui.horizontal(|ui| {
                ui.add(egui::DragValue::new(&mut config.pan_tuning).range(0.0..=1.0).speed(0.05));
                ui.label("(0 = off (default); higher = more mouse travel per pixel when zoomed in, less when zoomed out)");
            });
            ui.end_row();

            ui.label("Shift slow factor");
            ui.horizontal(|ui| {
                ui.add(
                    egui::DragValue::new(&mut config.shift_slow_factor)
                        .range(0.01..=1.0)
                        .speed(0.01),
                );
                ui.label("(0.1 = 10× slower while Shift is held; 1.0 = disabled)");
            });
            ui.end_row();

            ui.label("Show Shift indicator");
            ui.checkbox(&mut config.show_shift_osd, "");
            ui.end_row();

            ui.label("Hold-to-zoom speed");
            ui.horizontal(|ui| {
                ui.add(
                    egui::DragValue::new(&mut config.hold_to_zoom_speed)
                        .range(0.001..=1.0)
                        .speed(0.005),
                );
                ui.label("(zoom change per pixel of vertical motion)");
            });
            ui.end_row();

            ui.label("Screenshot path");
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut config.screenshot_path).desired_width(280.0),
                );
                if ui.button("Browse…").clicked() {
                    file_dialog.pick_directory();
                }
            });
            ui.end_row();

            ui.label("Screenshot filename pattern");
            ui.add(
                egui::TextEdit::singleline(&mut config.screenshot_filename_pattern)
                    .desired_width(320.0),
            );
            ui.end_row();

            ui.label("Screenshot selection color");
            ui.horizontal(|ui| {
                ui.color_edit_button_srgb(&mut config.screenshot_selection_color);
                ui.label("(border color of the selection rectangle in screenshot mode)");
            });
            ui.end_row();

            ui.label("Default screenshot scale");
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("screenshot_scale")
                    .selected_text(config.screenshot_scale.name())
                    .show_ui(ui, |ui| {
                        for mode in [
                            crate::config::ScreenshotScale::Real,
                            crate::config::ScreenshotScale::Magnified,
                        ] {
                            ui.selectable_value(
                                &mut config.screenshot_scale,
                                mode,
                                mode.name(),
                            );
                        }
                    });
                ui.label("(real = saved pixels, magnified = scaled to current zoom)");
            });
            ui.end_row();
        });

    // The default zoom may never exceed the max zoom, and never sit below the
    // configured minimum (1x) unless 0 % zoom is allowed — except that a
    // default of exactly 0 % auto-enables the allow-zero option (handled
    // above) so it is always reachable.
    if let Some(dz) = config.default_zoom.as_mut() {
        let min_dz = if config.allow_zero_zoom || *dz == 0.0 {
            0.0
        } else {
            1.0
        };
        *dz = (*dz).clamp(min_dz, config.max_zoom);
    }

    ui.add_space(10.0);

    ui.strong("Layout");
    ui.add_space(4.0);
    egui::Grid::new("layout_grid")
        .num_columns(2)
        .spacing([16.0, 8.0])
        .show(ui, |ui| {
            ui.label("OSD legend corner");
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("osd_corner")
                    .selected_text(config.osd_corner.to_string())
                    .show_ui(ui, |ui| {
                        for corner in [
                            crate::osd::Corner::TopLeft,
                            crate::osd::Corner::TopRight,
                            crate::osd::Corner::BottomLeft,
                            crate::osd::Corner::BottomRight,
                        ] {
                            ui.selectable_value(&mut config.osd_corner, corner, corner.to_string());
                        }
                    });
            });
            ui.end_row();
            ui.label("Minimap corner");
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("minimap_corner")
                    .selected_text(config.minimap_corner.to_string())
                    .show_ui(ui, |ui| {
                        for corner in [
                            crate::osd::Corner::TopLeft,
                            crate::osd::Corner::TopRight,
                            crate::osd::Corner::BottomLeft,
                            crate::osd::Corner::BottomRight,
                        ] {
                            ui.selectable_value(
                                &mut config.minimap_corner,
                                corner,
                                corner.to_string(),
                            );
                        }
                    });
            });
            ui.end_row();
            ui.label("Minimap outline style");
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("minimap_outline_scheme")
                    .selected_text(config.minimap_outline_scheme.name())
                    .show_ui(ui, |ui| {
                        for scheme in [
                            crate::config::MinimapOutlineScheme::Gradient,
                            crate::config::MinimapOutlineScheme::AngularGradient,
                            crate::config::MinimapOutlineScheme::MarchingAnts,
                        ] {
                            ui.selectable_value(
                                &mut config.minimap_outline_scheme,
                                scheme,
                                scheme.name(),
                            );
                        }
                    });
                ui.label("(animated border of the minimap panel)");
            });
            ui.end_row();
            ui.label("Outline animation speed");
            ui.horizontal(|ui| {
                ui.add(
                    egui::DragValue::new(&mut config.minimap_outline_speed)
                        .range(0.01..=10.0)
                        .speed(0.05),
                );
                ui.label("(0.2 = default, 5× slower than 1.0)");
            });
            ui.end_row();
            ui.label("Outline thickness (px)");
            ui.horizontal(|ui| {
                ui.add(
                    egui::DragValue::new(&mut config.minimap_outline_thickness)
                        .range(1..=8)
                        .speed(1),
                );
                ui.label("(3 = default)");
            });
            ui.end_row();
            ui.label("Outline zoom thickening");
            ui.horizontal(|ui| {
                ui.add(
                    egui::DragValue::new(&mut config.minimap_outline_zoom_scale)
                        .range(0.0..=2.0)
                        .speed(0.05),
                );
                ui.label("(0.25 = 25 % thicker at max zoom; 0 = constant width)");
            });
            ui.end_row();
        });

    ui.add_space(10.0);
    ui.separator();
    ui.add_space(10.0);
    ui.strong("Keybindings");
    ui.add_space(4.0);

    egui::Grid::new("keybindings_grid")
        .num_columns(2)
        .spacing([16.0, 8.0])
        .show(ui, |ui| {
            key_row(ui, "Toggle OSD", &mut config.keybindings.toggle_osd);
            key_row(
                ui,
                "Screenshot fullscreen",
                &mut config.keybindings.screenshot_fullscreen,
            );
            key_row(
                ui,
                "Screenshot manual selection",
                &mut config.keybindings.screenshot_manual,
            );
            key_row(
                ui,
                "Screenshot window",
                &mut config.keybindings.screenshot_window,
            );
            key_row(
                ui,
                "Toggle screenshot scale",
                &mut config.keybindings.screenshot_scale_toggle,
            );
            key_row(
                ui,
                "Configuration window",
                &mut config.keybindings.config_window,
            );
            key_row(ui, "Reset zoom", &mut config.keybindings.reset_zoom);
            key_row(
                ui,
                "Toggle magnified cursor",
                &mut config.keybindings.toggle_cursor,
            );
            key_row(ui, "Toggle minimap", &mut config.keybindings.minimap);
            key_row(ui, "Hold-to-zoom key", &mut config.keybindings.hold_to_zoom);
            key_row(
                ui,
                "Toggle anti-aliasing",
                &mut config.keybindings.anti_aliasing,
            );
            key_row(
                ui,
                "Enter annotation mode",
                &mut config.keybindings.mode_annotation,
            );
            key_row(
                ui,
                "Enter capture mode",
                &mut config.keybindings.mode_capture,
            );
        });

    ui.add_space(6.0);
    ui.small(
        "Close this window with the Close button or Escape; the magnifier's keys (zoom 0-9, R reset, OSD, screenshots) resume immediately. Runtime changes apply live and are auto-saved on change.",
    );
    ui.small("Numeric keys 0-9 set zoom levels: 0 = 0%, keys 1-9 are percentages of the max zoom.");
    ui.small("Q / Esc / RMB quit Maggie when the Configuration window is closed.");
    ui.small("The reset-zoom key (default R) resets the zoom to the default.");
    ui.small("Hold-to-zoom: hold the key (default MMB, or a modifier like Super) and move the mouse up/down to zoom smoothly; left/right motion still pans normally.");
    ui.small("If the hold-to-zoom key is a letter that is also bound to another action, both trigger — keep it on a modifier or mouse button.");
}

fn key_row(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.label(label);
    ui.add(egui::TextEdit::singleline(value).desired_width(140.0));
    ui.end_row();
}

fn scroll_zoom_name(mode: crate::config::ScrollZoomMode) -> &'static str {
    match mode {
        crate::config::ScrollZoomMode::Levels => "levels",
        crate::config::ScrollZoomMode::Factor => "factor",
    }
}

// ----------------------------------------------------------------------
// Keysym -> egui::Key mapping (xkeysym keysyms, as re-exported by sctk)
// ----------------------------------------------------------------------

pub fn keysym_to_egui_key(
    keysym: crate::platform::wayland::Keysym,
) -> Option<egui::Key> {
    use crate::platform::wayland::Keysym as K;

    // Keys whose xkb name does not match egui's `from_name` vocabulary.
    let special = match keysym {
        K::Escape => Some(egui::Key::Escape),
        K::Return => Some(egui::Key::Enter),
        K::Tab => Some(egui::Key::Tab),
        K::BackSpace => Some(egui::Key::Backspace),
        K::Delete => Some(egui::Key::Delete),
        K::Home => Some(egui::Key::Home),
        K::End => Some(egui::Key::End),
        K::Page_Up => Some(egui::Key::PageUp),
        K::Page_Down => Some(egui::Key::PageDown),
        K::Insert => Some(egui::Key::Insert),
        K::Left => Some(egui::Key::ArrowLeft),
        K::Right => Some(egui::Key::ArrowRight),
        K::Up => Some(egui::Key::ArrowUp),
        K::Down => Some(egui::Key::ArrowDown),
        K::space => Some(egui::Key::Space),
        K::minus => Some(egui::Key::Minus),
        K::plus => Some(egui::Key::Plus),
        _ => None,
    };
    if let Some(key) = special {
        return Some(key);
    }

    // Printable ASCII keysyms (letters, digits, punctuation) map to the char.
    let value = u32::from(keysym);
    if (0x21..=0x7E).contains(&value)
        && let Some(c) = char::from_u32(value)
        && let Some(key) = egui::Key::from_name(&c.to_string())
    {
        return Some(key);
    }

    // F-keys and anything else via the keysym name (xkeysym reports
    // "XK_F1", "XK_Escape", ...; strip the prefix for egui's vocabulary).
    let name = keysym
        .name()
        .unwrap_or("")
        .strip_prefix("XK_")
        .unwrap_or("");
    egui::Key::from_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keysym_mapping_covers_common_keys() {
        use crate::platform::wayland::Keysym as K;
        assert_eq!(keysym_to_egui_key(K::Escape), Some(egui::Key::Escape));
        assert_eq!(keysym_to_egui_key(K::Return), Some(egui::Key::Enter));
        assert_eq!(keysym_to_egui_key(K::Tab), Some(egui::Key::Tab));
        assert_eq!(keysym_to_egui_key(K::BackSpace), Some(egui::Key::Backspace));
        assert_eq!(keysym_to_egui_key(K::Left), Some(egui::Key::ArrowLeft));
        assert_eq!(keysym_to_egui_key(K::Right), Some(egui::Key::ArrowRight));
        assert_eq!(keysym_to_egui_key(K::Up), Some(egui::Key::ArrowUp));
        assert_eq!(keysym_to_egui_key(K::Down), Some(egui::Key::ArrowDown));
        assert_eq!(keysym_to_egui_key(K::a), Some(egui::Key::A));
        assert_eq!(keysym_to_egui_key(K::_1), Some(egui::Key::Num1));
        assert_eq!(keysym_to_egui_key(K::space), Some(egui::Key::Space));
        assert_eq!(keysym_to_egui_key(K::minus), Some(egui::Key::Minus));
        assert_eq!(keysym_to_egui_key(K::plus), Some(egui::Key::Plus));
        assert_eq!(keysym_to_egui_key(K::F1), Some(egui::Key::F1));
        // Control keys that egui does not use simply map to None.
        assert_eq!(keysym_to_egui_key(K::Shift_L), None);
    }
}
