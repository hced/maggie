#![allow(dead_code)]

#[repr(C)]
#[derive(Clone)]
pub struct RgbaBuffer {
    pub width: i32,
    pub height: i32,
    pub data: Vec<u8>,
}

impl RgbaBuffer {
    pub fn new(width: i32, height: i32) -> Self {
        RgbaBuffer {
            width,
            height,
            data: vec![0u8; (width * height * 4) as usize],
        }
    }

    pub fn pixel(&self, x: i32, y: i32) -> Option<[u8; 4]> {
        if x < 0 || y < 0 || x >= self.width || y >= self.height {
            return None;
        }
        let idx = ((y * self.width + x) * 4) as usize;
        Some([
            self.data[idx],
            self.data[idx + 1],
            self.data[idx + 2],
            self.data[idx + 3],
        ])
    }

    pub fn set_pixel(&mut self, x: i32, y: i32, rgba: [u8; 4]) {
        if x < 0 || y < 0 || x >= self.width || y >= self.height {
            return;
        }
        let idx = ((y * self.width + x) * 4) as usize;
        self.data[idx..idx + 4].copy_from_slice(&rgba);
    }

    pub fn nearest_neighbor_scale(&self, scale: f64) -> RgbaBuffer {
        let new_width = (self.width as f64 * scale).round() as i32;
        let new_height = (self.height as f64 * scale).round() as i32;

        let mut result = RgbaBuffer::new(new_width, new_height);

        for y in 0..new_height {
            for x in 0..new_width {
                let src_x = (x as f64 / scale).floor() as i32;
                let src_y = (y as f64 / scale).floor() as i32;

                if let Some(pixel) = self.pixel(src_x, src_y) {
                    result.set_pixel(x, y, pixel);
                }
            }
        }

        result
    }
}

pub struct Renderer {
    scale_factor: f64,
}

impl Renderer {
    pub fn new(scale_factor: f64) -> Self {
        Renderer { scale_factor }
    }

    pub fn render_nearest_neighbor(&mut self, source: &RgbaBuffer) -> RgbaBuffer {
        source.nearest_neighbor_scale(self.scale_factor)
    }

    /// Scale the view `origin` (capture px, may extend past the edges in
    /// `Extend` edge mode) to `dest_w`x`dest_h`, bilinear-filtered. Samples
    /// beyond the capture are either edge-stretched (`Stretch`) or painted
    /// black (`Black`).
    pub fn render_bilinear(
        &self,
        source: &RgbaBuffer,
        origin: (f64, f64),
        dest_w: i32,
        dest_h: i32,
        edge_fill: crate::config::HtzEdgeFill,
    ) -> RgbaBuffer {
        let zoom = self.scale_factor;
        let inv_zoom = 1.0 / zoom;
        let src_w = source.width;
        let src_h = source.height;
        let max_x = (src_w - 1).max(0) as f64;
        let max_y = (src_h - 1).max(0) as f64;
        let data = &source.data;
        let mut result = vec![0u8; (dest_w * dest_h * 4) as usize];

        let black = edge_fill == crate::config::HtzEdgeFill::Black;
        for y in 0..dest_h {
            let sy_raw = origin.1 + (y as f64 + 0.5) * inv_zoom;
            let out_row = y as usize * dest_w as usize;
            for x in 0..dest_w {
                let o = (out_row + x as usize) * 4;
                let sx_raw = origin.0 + (x as f64 + 0.5) * inv_zoom;
                if black && (sx_raw < 0.0 || sx_raw > max_x || sy_raw < 0.0 || sy_raw > max_y) {
                    continue; // leave black
                }
                let sy = sy_raw.clamp(0.0, max_y);
                let y0 = sy.floor() as i32;
                let y1 = (y0 + 1).min(src_h - 1);
                let fy = (sy - y0 as f64) as f32;
                let row0 = y0 as usize * src_w as usize;
                let row1 = y1 as usize * src_w as usize;
                let sx = sx_raw.clamp(0.0, max_x);
                let x0 = sx.floor() as i32;
                let x1 = (x0 + 1).min(src_w - 1);
                let fx = (sx - x0 as f64) as f32;
                let i00 = (row0 + x0 as usize) * 4;
                let i01 = (row0 + x1 as usize) * 4;
                let i10 = (row1 + x0 as usize) * 4;
                let i11 = (row1 + x1 as usize) * 4;
                let p00 = &data[i00..i00 + 4];
                let p01 = &data[i01..i01 + 4];
                let p10 = &data[i10..i10 + 4];
                let p11 = &data[i11..i11 + 4];
                for c in 0..4 {
                    let top = p00[c] as f32 + (p01[c] as f32 - p00[c] as f32) * fx;
                    let bottom = p10[c] as f32 + (p11[c] as f32 - p10[c] as f32) * fx;
                    result[o + c] = (top + (bottom - top) * fy).round() as u8;
                }
            }
        }

        RgbaBuffer {
            width: dest_w,
            height: dest_h,
            data: result,
        }
    }

    pub fn update_scale_factor(&mut self, new_scale: f64) {
        self.scale_factor = new_scale;
    }

    pub fn scale_factor(&self) -> f64 {
        self.scale_factor
    }
}
