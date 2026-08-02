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
        Some([self.data[idx], self.data[idx + 1], self.data[idx + 2], self.data[idx + 3]])
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

    pub fn update_scale_factor(&mut self, new_scale: f64) {
        self.scale_factor = new_scale;
    }

    pub fn scale_factor(&self) -> f64 {
        self.scale_factor
    }
}
