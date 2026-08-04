use crate::render::RgbaBuffer;
use std::path::PathBuf;

/// Base edge length (logical px at 1x zoom) of the fallback reticle sprite.
const CURSOR_SIZE_BASE: i32 = 16;
/// Half-size (logical px at 1x zoom) of the opaque black center square.
const CURSOR_CENTER_HALF: i32 = 4;
/// Thickness (logical px at 1x zoom) of the white ring around the center.
const CURSOR_RING_THICKNESS: i32 = 2;

/// The magnified cursor shown inside the viewport.
///
/// On startup this loads the **real system cursor** (the compositor's cursor
/// theme) at its native resolution and scales it with the zoom level, so the
/// magnified cursor looks exactly like the cursor the user sees outside the
/// viewport. Wayland gives clients no protocol to *read* the compositor's
/// cursor bitmap, so the theme is loaded from the cursor file on disk
/// (`$XCURSOR_THEME` / `$XCURSOR_SIZE` and the standard icon search paths).
///
/// If no theme can be found, a stylized reticle (white ring around a black
/// center) is used as a fallback.
pub struct MagnifiedCursor {
    /// Base cursor image at its native size, straight-alpha RGBA.
    base: RgbaBuffer,
    /// Hotspot (the pointer's anchor point) in base-image coordinates.
    hotspot: (f64, f64),
    zoom: f64,
    /// Cache of the last rendered sprite: (scale factor, sprite, hotspot).
    /// Rebuilt only when the zoom or render scale changes, so mouse motion
    /// doesn't re-run the upscale on every event.
    cache: Option<(f64, RgbaBuffer, (f64, f64))>,
}

impl MagnifiedCursor {
    pub fn new(zoom: f64) -> Self {
        match load_system_cursor() {
            Some((base, hotspot)) => Self {
                base,
                hotspot,
                zoom,
                cache: None,
            },
            None => Self::from_reticle(zoom),
        }
    }

    /// Build a cursor from a pre-rendered reticle. Used as the fallback when
    /// no system theme is available, and by tests.
    pub(crate) fn from_reticle(zoom: f64) -> Self {
        let base = build_reticle(CURSOR_SIZE_BASE);
        let hotspot = (base.width as f64 / 2.0, base.height as f64 / 2.0);
        Self {
            base,
            hotspot,
            zoom,
            cache: None,
        }
    }

    pub fn update_zoom(&mut self, new_zoom: f64) {
        if (new_zoom - self.zoom).abs() > 1e-9 {
            self.zoom = new_zoom;
            self.cache = None;
        }
    }

    /// Render the magnified cursor. `scale` is the render-buffer multiplier
    /// (1.0 on the CPU path, `RENDER_SCALE` on the GPU path). Returns the
    /// sprite and its hotspot in sprite-pixel coordinates, so callers can
    /// place the pointer's exact tip on the content under the cursor.
    ///
    /// The sprite is cached per scale factor: mouse motion reuses the cached
    /// image instead of re-upscaling the base bitmap every event.
    pub fn sprite(&mut self, scale: f64) -> (RgbaBuffer, (f64, f64)) {
        let factor = self.zoom * scale;
        if let Some((f, sprite, hotspot)) = &self.cache
            && (*f - factor).abs() < 1e-9
        {
            return (sprite.clone(), *hotspot);
        }
        let sprite = self.base.nearest_neighbor_scale(factor);
        let hotspot = (self.hotspot.0 * factor, self.hotspot.1 * factor);
        self.cache = Some((factor, sprite.clone(), hotspot));
        (sprite, hotspot)
    }

    /// Convenience for tests: build a `MagnifiedCursor` from an explicit image
    /// (also used by the engine tests, hence `pub(crate)`).
    #[cfg(test)]
    pub(crate) fn from_parts_for_test(base: RgbaBuffer, hotspot: (f64, f64)) -> Self {
        Self {
            base,
            hotspot,
            zoom: 1.0,
            cache: None,
        }
    }
}

/// Fallback reticle: a white ring around a black center, like a classic
/// target-style cursor, at the given base edge length.
fn build_reticle(base_size: i32) -> RgbaBuffer {
    let (width, height) = (base_size.max(1), base_size.max(1));
    let mut buffer = RgbaBuffer::new(width, height);

    let center_half = CURSOR_CENTER_HALF.max(1);
    let ring_outer = center_half + CURSOR_RING_THICKNESS.max(1);
    let center_x = width / 2;
    let center_y = height / 2;

    // Outer ring (white border).
    for y in 0..height {
        for x in 0..width {
            let dx = (x - center_x).abs();
            let dy = (y - center_y).abs();
            let inside_ring = dx < ring_outer && dy < ring_outer;
            let outside_center = dx >= center_half || dy >= center_half;
            if inside_ring && outside_center {
                buffer.set_pixel(x, y, [255, 255, 255, 255]);
            }
        }
    }

    // Inner square (black dot).
    for y in 0..height {
        for x in 0..width {
            let dx = (x - center_x).abs();
            let dy = (y - center_y).abs();
            if dx < center_half && dy < center_half {
                buffer.set_pixel(x, y, [0, 0, 0, 255]);
            }
        }
    }

    buffer
}

/// Load the system cursor theme's "default" pointer (trying several common
/// names) at the requested nominal size, returning the image and its hotspot.
fn load_system_cursor() -> Option<(RgbaBuffer, (f64, f64))> {
    let theme = std::env::var("XCURSOR_THEME").unwrap_or_else(|_| "default".to_string());
    let size: u32 = std::env::var("XCURSOR_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(24);

    for name in ["default", "left_ptr", "arrow", "top_left_arrow"] {
        if let Some(loaded) = load_cursor(&theme, name, size) {
            tracing::info!("Loaded system cursor '{name}' (nominal {size}px) for magnification");
            return Some(loaded);
        }
    }
    tracing::warn!("No system cursor theme found; falling back to the reticle");
    None
}

/// Locate `<theme>/cursors/<name>` on the standard icon search paths.
fn find_cursor_file(theme: &str, name: &str) -> Option<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(path) = std::env::var("XCURSOR_PATH") {
        roots.extend(path.split(':').filter(|s| !s.is_empty()).map(PathBuf::from));
    }
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join(".icons"));
    }
    if let Ok(xdg_data_home) = std::env::var("XDG_DATA_HOME") {
        roots.push(PathBuf::from(xdg_data_home).join("icons"));
    } else if let Some(home) = dirs::home_dir() {
        roots.push(home.join(".local/share/icons"));
    }
    if let Ok(dirs_env) = std::env::var("XDG_DATA_DIRS") {
        roots.extend(
            dirs_env
                .split(':')
                .filter(|s| !s.is_empty())
                .map(|d| PathBuf::from(d).join("icons")),
        );
    } else {
        roots.push(PathBuf::from("/usr/local/share/icons"));
        roots.push(PathBuf::from("/usr/share/icons"));
    }

    let mut themes = vec![theme.to_string()];
    if theme != "default" {
        themes.push("default".to_string());
    }
    themes.push("Adwaita".to_string());
    themes.push("DMZ-White".to_string());

    for t in themes {
        for root in &roots {
            let candidate = root.join(&t).join("cursors").join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Read and parse a cursor file, converting the nearest-size image into a
/// straight-alpha RGBA buffer (the file stores premultiplied ARGB pixel data).
fn load_cursor(theme: &str, name: &str, size: u32) -> Option<(RgbaBuffer, (f64, f64))> {
    let path = find_cursor_file(theme, name)?;
    let bytes = std::fs::read(path).ok()?;
    let images = xcursor::parser::parse_xcursor(&bytes)?;
    let img = images
        .iter()
        .min_by_key(|im| (size as i64 - im.size as i64).abs())?;
    Some(image_to_buffer(img))
}

/// Convert a parsed XCursor image into a straight-alpha RGBA buffer plus its
/// hotspot. The file stores premultiplied ARGB pixel data (some themes store
/// straight alpha); un-premultiply it so straight-alpha blending is correct.
fn image_to_buffer(img: &xcursor::parser::Image) -> (RgbaBuffer, (f64, f64)) {
    let width = img.width as i32;
    let height = img.height as i32;
    let mut buffer = RgbaBuffer::new(width, height);
    for (idx, chunk) in img.pixels_rgba.chunks_exact(4).enumerate() {
        let (r, g, b, a) = (chunk[0], chunk[1], chunk[2], chunk[3]);
        let rgba = if a == 0 {
            [0, 0, 0, 0]
        } else if r.max(g).max(b) > a {
            // Not premultiplied (some themes store straight alpha).
            [r, g, b, a]
        } else {
            // Premultiplied: un-premultiply for straight-alpha blending.
            [
                (r as u32 * 255 / a as u32) as u8,
                (g as u32 * 255 / a as u32) as u8,
                (b as u32 * 255 / a as u32) as u8,
                a,
            ]
        };
        let x = idx as i32 % width;
        let y = idx as i32 / width;
        buffer.set_pixel(x, y, rgba);
    }
    (buffer, (img.xhot as f64, img.yhot as f64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reticle_has_white_ring_and_black_center() {
        let buf = build_reticle(16);
        assert_eq!((buf.width, buf.height), (16, 16));

        let center = (buf.width / 2, buf.height / 2);
        assert_eq!(buf.pixel(center.0, center.1), Some([0, 0, 0, 255]));

        // A pixel on the ring (5 px right of center: inside ring_outer=6,
        // outside center_half=4) must be white.
        let ring_px = buf.pixel(center.0 + 5, center.1);
        assert_eq!(ring_px, Some([255, 255, 255, 255]));

        // Corners (beyond the ring) stay transparent.
        assert_eq!(buf.pixel(0, 0), Some([0, 0, 0, 0]));
    }

    #[test]
    fn sprite_scales_reticle_with_zoom() {
        let mut cursor = MagnifiedCursor::from_reticle(2.0);
        let (sprite, hotspot) = cursor.sprite(1.0);
        assert_eq!((sprite.width, sprite.height), (32, 32));
        // Reticle hotspot is its center, scaled by the zoom factor.
        assert_eq!(hotspot, (16.0, 16.0));

        let center = (sprite.width / 2, sprite.height / 2);
        assert_eq!(sprite.pixel(center.0, center.1), Some([0, 0, 0, 255]));
        // Ring half-size at zoom 2: center_half 8 + thickness 4 = 12, so
        // dx=11 is on the ring and dx=13 is outside it.
        assert_eq!(
            sprite.pixel(center.0 + 11, center.1),
            Some([255, 255, 255, 255])
        );
        assert_eq!(sprite.pixel(center.0 + 13, center.1), Some([0, 0, 0, 0]));
    }

    #[test]
    fn sprite_scales_with_render_scale() {
        let mut cursor = MagnifiedCursor::from_reticle(1.0);
        let (sprite, _) = cursor.sprite(2.0);
        assert_eq!((sprite.width, sprite.height), (32, 32));
    }

    #[test]
    fn sprite_scales_hotspot_with_zoom() {
        // A synthetic 4x4 cursor with hotspot at (1, 2), zoomed 3x, must
        // produce a 12x12 sprite with hotspot at (3, 6).
        let mut base = RgbaBuffer::new(4, 4);
        base.set_pixel(1, 2, [255, 0, 0, 255]);
        let mut cursor = MagnifiedCursor::from_parts_for_test(base, (1.0, 2.0));
        cursor.update_zoom(3.0);
        let (sprite, hotspot) = cursor.sprite(1.0);
        assert_eq!((sprite.width, sprite.height), (12, 12));
        assert_eq!(hotspot, (3.0, 6.0));
    }

    #[test]
    fn premultiplied_pixels_are_unmultiplied() {
        // Premultiplied red at 50% alpha: file bytes [r,g,b,a] = [128, 0, 0, 128].
        let image = xcursor::parser::Image {
            size: 1,
            width: 1,
            height: 1,
            xhot: 0,
            yhot: 0,
            delay: 0,
            pixels_rgba: vec![128, 0, 0, 128],
            pixels_argb: vec![],
        };
        let (buf, hotspot) = image_to_buffer(&image);
        assert_eq!(buf.pixel(0, 0), Some([255, 0, 0, 128]));
        assert_eq!(hotspot, (0.0, 0.0));
    }

    #[test]
    fn straight_alpha_pixels_are_kept() {
        // Straight alpha data has color channels above the alpha byte.
        let image = xcursor::parser::Image {
            size: 1,
            width: 1,
            height: 1,
            xhot: 0,
            yhot: 0,
            delay: 0,
            pixels_rgba: vec![255, 0, 0, 128],
            pixels_argb: vec![],
        };
        let (buf, _) = image_to_buffer(&image);
        assert_eq!(buf.pixel(0, 0), Some([255, 0, 0, 128]));
    }
}
