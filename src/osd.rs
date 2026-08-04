#![allow(dead_code)]

const GLYPH_WIDTH: i32 = 5;
const GLYPH_HEIGHT: usize = 7;
const GLYPH_SCALE: i32 = 2;
const GLYPH_ADVANCE: i32 = 6 * GLYPH_SCALE;
const LINE_HEIGHT: i32 = (GLYPH_HEIGHT as i32 + 2) * GLYPH_SCALE;
const BOX_PADDING: i32 = 16;
const BOX_MARGIN: i32 = 14;
const BOX_ALPHA: u8 = 255;
const TEXT_COLOR: [u8; 3] = [0xE6, 0xE6, 0xE6];

fn glyph(character: char) -> Option<&'static [u8; GLYPH_HEIGHT]> {
    match character {
        ' ' => Some(&[
            0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000,
        ]),
        '0' => Some(&[
            0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
        ]),
        '1' => Some(&[
            0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ]),
        '2' => Some(&[
            0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111,
        ]),
        '3' => Some(&[
            0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110,
        ]),
        '4' => Some(&[
            0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
        ]),
        '5' => Some(&[
            0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
        ]),
        '6' => Some(&[
            0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
        ]),
        '7' => Some(&[
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
        ]),
        '8' => Some(&[
            0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
        ]),
        '9' => Some(&[
            0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100,
        ]),
        'a' | 'A' => Some(&[
            0b00000, 0b00000, 0b01110, 0b00001, 0b01111, 0b10001, 0b01111,
        ]),
        'b' | 'B' => Some(&[
            0b10000, 0b10000, 0b10110, 0b11001, 0b10001, 0b10001, 0b01110,
        ]),
        'c' | 'C' => Some(&[
            0b00000, 0b00000, 0b01110, 0b10000, 0b10000, 0b10001, 0b01110,
        ]),
        'd' | 'D' => Some(&[
            0b00001, 0b00001, 0b01101, 0b10011, 0b10001, 0b10001, 0b01111,
        ]),
        'e' | 'E' => Some(&[
            0b00000, 0b00000, 0b01110, 0b10001, 0b11111, 0b10000, 0b01110,
        ]),
        'f' | 'F' => Some(&[
            0b00110, 0b01001, 0b01000, 0b11100, 0b01000, 0b01000, 0b01000,
        ]),
        'g' | 'G' => Some(&[
            0b00000, 0b01111, 0b10001, 0b10001, 0b01111, 0b00001, 0b01110,
        ]),
        'h' | 'H' => Some(&[
            0b10000, 0b10000, 0b10110, 0b11001, 0b10001, 0b10001, 0b10001,
        ]),
        'i' | 'I' => Some(&[
            0b00100, 0b00000, 0b01100, 0b00100, 0b00100, 0b00100, 0b01110,
        ]),
        'j' | 'J' => Some(&[
            0b00010, 0b00000, 0b00110, 0b00010, 0b00010, 0b10010, 0b01100,
        ]),
        'k' | 'K' => Some(&[
            0b10000, 0b10000, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010,
        ]),
        'l' | 'L' => Some(&[
            0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ]),
        'm' | 'M' => Some(&[
            0b00000, 0b00000, 0b11010, 0b10101, 0b10101, 0b10101, 0b10101,
        ]),
        'n' | 'N' => Some(&[
            0b00000, 0b00000, 0b10110, 0b11001, 0b10001, 0b10001, 0b10001,
        ]),
        'o' | 'O' => Some(&[
            0b00000, 0b00000, 0b01110, 0b10001, 0b10001, 0b10001, 0b01110,
        ]),
        'p' | 'P' => Some(&[
            0b00000, 0b00000, 0b10110, 0b11001, 0b10001, 0b11110, 0b10000,
        ]),
        'q' | 'Q' => Some(&[
            0b00000, 0b00000, 0b01101, 0b10011, 0b10001, 0b01111, 0b00001,
        ]),
        'r' | 'R' => Some(&[
            0b00000, 0b00000, 0b10110, 0b11001, 0b10000, 0b10000, 0b10000,
        ]),
        's' | 'S' => Some(&[
            0b00000, 0b00000, 0b01111, 0b10000, 0b01110, 0b00001, 0b11110,
        ]),
        't' | 'T' => Some(&[
            0b01000, 0b01000, 0b11100, 0b01000, 0b01000, 0b01001, 0b00110,
        ]),
        'u' | 'U' => Some(&[
            0b00000, 0b00000, 0b10001, 0b10001, 0b10001, 0b10011, 0b01101,
        ]),
        'v' | 'V' => Some(&[
            0b00000, 0b00000, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100,
        ]),
        'w' | 'W' => Some(&[
            0b00000, 0b00000, 0b10101, 0b10101, 0b10101, 0b10101, 0b01010,
        ]),
        'x' | 'X' => Some(&[
            0b00000, 0b00000, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001,
        ]),
        'y' | 'Y' => Some(&[
            0b00000, 0b00000, 0b10001, 0b10001, 0b10001, 0b01111, 0b00001,
        ]),
        'z' | 'Z' => Some(&[
            0b00000, 0b00000, 0b11111, 0b00010, 0b00100, 0b01000, 0b11111,
        ]),
        '-' => Some(&[
            0b00000, 0b00000, 0b00000, 0b11111, 0b00000, 0b00000, 0b00000,
        ]),
        '/' => Some(&[
            0b00001, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b10000,
        ]),
        '+' => Some(&[
            0b00000, 0b00100, 0b00100, 0b11111, 0b00100, 0b00100, 0b00000,
        ]),
        '.' => Some(&[
            0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b01100, 0b01100,
        ]),
        ':' => Some(&[
            0b00000, 0b01100, 0b01100, 0b00000, 0b01100, 0b01100, 0b00000,
        ]),
        _ => None,
    }
}

pub fn draw_text(
    canvas: &mut [u8],
    canvas_w: i32,
    canvas_h: i32,
    x: i32,
    y: i32,
    text: &str,
    color: [u8; 3],
) {
    let mut pen_x = x;
    for character in text.chars() {
        if let Some(rows) = glyph(character) {
            for (row_index, row) in rows.iter().enumerate() {
                for col in 0..GLYPH_WIDTH {
                    if row & (1 << (GLYPH_WIDTH - 1 - col)) == 0 {
                        continue;
                    }
                    for dy in 0..GLYPH_SCALE {
                        let py = y + row_index as i32 * GLYPH_SCALE + dy;
                        if py < 0 || py >= canvas_h {
                            continue;
                        }
                        for dx in 0..GLYPH_SCALE {
                            let px = pen_x + col * GLYPH_SCALE + dx;
                            if px < 0 || px >= canvas_w {
                                continue;
                            }
                            let index = ((py * canvas_w + px) * 4) as usize;
                            canvas[index] = color[2];
                            canvas[index + 1] = color[1];
                            canvas[index + 2] = color[0];
                            canvas[index + 3] = 255;
                        }
                    }
                }
            }
        }
        pen_x += GLYPH_ADVANCE;
    }
}

pub fn draw_osd(
    canvas: &mut [u8],
    canvas_w: i32,
    canvas_h: i32,
    lines: &[String],
    cursor: (i32, i32),
) {
    let Some(sprite) = build_osd_sprite(lines, cursor, canvas_w, canvas_h) else {
        return;
    };

    for y in sprite.y..(sprite.y + sprite.height) {
        if y < 0 || y >= canvas_h {
            continue;
        }
        let src_row =
            &sprite.buffer.data[((y - sprite.y) as usize) * (sprite.width as usize) * 4..];
        let dst_start = ((y * canvas_w + sprite.x.max(0)) as usize) * 4;
        let dst_end = ((y * canvas_w + (sprite.x + sprite.width).min(canvas_w)) as usize) * 4;
        if dst_end <= dst_start {
            continue;
        }
        let skip = (sprite.x.max(0) - sprite.x) as usize;
        for (dst_px, src_px) in canvas[dst_start..dst_end]
            .chunks_exact_mut(4)
            .zip(src_row[skip * 4..].chunks_exact(4))
        {
            if src_px[3] == 0 {
                continue;
            }
            dst_px.copy_from_slice(src_px);
        }
    }
}

/// An OSD legend rendered into its own tight buffer, ready for compositing at
/// `(x, y)` on a `screen_w x screen_h` surface.
pub struct OsdSprite {
    pub buffer: crate::render::RgbaBuffer,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

pub fn build_osd_sprite(
    lines: &[String],
    cursor: (i32, i32),
    screen_w: i32,
    screen_h: i32,
) -> Option<OsdSprite> {
    if lines.is_empty() || screen_w <= 0 || screen_h <= 0 {
        return None;
    }

    let widest = lines
        .iter()
        .map(|line| line.chars().count() as i32 * GLYPH_ADVANCE)
        .max()
        .unwrap_or(0);

    let box_w = widest + BOX_PADDING;
    let box_h = lines.len() as i32 * LINE_HEIGHT + BOX_PADDING;

    let corners = [
        (BOX_MARGIN, BOX_MARGIN),
        (screen_w - BOX_MARGIN - box_w, BOX_MARGIN),
        (BOX_MARGIN, screen_h - BOX_MARGIN - box_h),
        (screen_w - BOX_MARGIN - box_w, screen_h - BOX_MARGIN - box_h),
    ];
    let (box_x, box_y) = corners
        .into_iter()
        .max_by(|a, b| {
            let da =
                ((a.0 + box_w / 2 - cursor.0) as f64).hypot((a.1 + box_h / 2 - cursor.1) as f64);
            let db =
                ((b.0 + box_w / 2 - cursor.0) as f64).hypot((b.1 + box_h / 2 - cursor.1) as f64);
            da.total_cmp(&db)
        })
        .unwrap();

    let mut buffer = crate::render::RgbaBuffer {
        width: box_w,
        height: box_h,
        data: vec![0u8; (box_w * box_h * 4) as usize],
    };

    let pad = BOX_PADDING / 2;
    for row in buffer.data.chunks_exact_mut((box_w * 4) as usize) {
        for pixel in row.chunks_exact_mut(4) {
            pixel[0] = 0;
            pixel[1] = 0;
            pixel[2] = 0;
            pixel[3] = BOX_ALPHA;
        }
    }

    for (index, line) in lines.iter().enumerate() {
        draw_text(
            &mut buffer.data,
            box_w,
            box_h,
            pad,
            pad + index as i32 * LINE_HEIGHT,
            line,
            TEXT_COLOR,
        );
    }

    Some(OsdSprite {
        buffer,
        x: box_x,
        y: box_y,
        width: box_w,
        height: box_h,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draw_text_writes_glyph_pixels() {
        let mut canvas = vec![0u8; 20 * 20 * 4];
        draw_text(&mut canvas, 20, 20, 0, 0, "0", [230, 230, 230]);
        let on_pixels = canvas.chunks_exact(4).filter(|p| p[3] == 255).count();
        assert!(on_pixels > 10, "expected glyph pixels, got {on_pixels}");
        assert_eq!(
            canvas
                .chunks_exact(4)
                .filter(|p| p[3] != 0 && p[3] != 255)
                .count(),
            0
        );
    }

    #[test]
    fn draw_text_clips_out_of_bounds() {
        let mut canvas = vec![0u8; 20 * 20 * 4];
        draw_text(
            &mut canvas,
            20,
            20,
            100,
            100,
            "Hello OSD 123",
            [255, 255, 255],
        );
        assert!(canvas.iter().all(|&b| b == 0));
        let mut canvas = vec![0u8; 20 * 20 * 4];
        draw_text(&mut canvas, 20, 20, 19, 19, "H", [255, 255, 255]);
        let on = canvas.chunks_exact(4).filter(|p| p[3] == 255).count();
        assert!(
            on > 0 && on <= 7,
            "only in-bounds glyph pixels drawn, got {on}"
        );
    }

    #[test]
    fn draw_osd_places_box_and_text() {
        let mut canvas = vec![0u8; 200 * 200 * 4];
        let lines = vec![
            "maggie  zoom 3x".to_string(),
            "1-9  zoom level".to_string(),
            "k  toggle OSD".to_string(),
        ];
        draw_osd(&mut canvas, 200, 200, &lines, (150, 150));
        let pixels: Vec<[u8; 4]> = canvas
            .chunks_exact(4)
            .map(|p| [p[0], p[1], p[2], p[3]])
            .collect();
        assert!(
            pixels.iter().any(|p| p[3] == BOX_ALPHA && p[0] == 0),
            "box not drawn"
        );
        assert!(
            pixels.iter().any(|p| p[3] == 255 && p[2] > 200),
            "text not drawn"
        );
        let text_ys: Vec<i32> = (0..200)
            .filter(|&y| {
                (0..200).any(|x| {
                    let p = &pixels[(y * 200 + x) as usize];
                    p[3] == 255 && p[2] > 200
                })
            })
            .collect();
        assert!(
            *text_ys.iter().min().unwrap() < 200 / 2,
            "box should be top-left"
        );
    }

    #[test]
    fn draw_osd_moves_bottom_right_when_cursor_top_left() {
        let mut canvas = vec![0u8; 200 * 200 * 4];
        let lines = vec!["k  toggle OSD".to_string()];
        draw_osd(&mut canvas, 200, 200, &lines, (10, 10));
        let pixels: Vec<[u8; 4]> = canvas
            .chunks_exact(4)
            .map(|p| [p[0], p[1], p[2], p[3]])
            .collect();
        let text_ys: Vec<i32> = (0..200)
            .filter(|&y| {
                (0..200).any(|x| {
                    let p = &pixels[(y * 200 + x) as usize];
                    p[3] == 255 && p[2] > 200
                })
            })
            .collect();
        assert!(
            *text_ys.iter().min().unwrap() > 200 / 2,
            "box should be bottom-right"
        );
    }

    #[test]
    fn draw_osd_picks_farthest_corner() {
        let corners = [
            ("top-left", (150, 150)),
            ("top-right", (10, 150)),
            ("bottom-left", (150, 10)),
            ("bottom-right", (10, 10)),
        ];
        for (name, cursor) in corners {
            let mut canvas = vec![0u8; 200 * 200 * 4];
            let lines = vec!["zoom".to_string()];
            draw_osd(&mut canvas, 200, 200, &lines, cursor);
            let pixels: Vec<[u8; 4]> = canvas
                .chunks_exact(4)
                .map(|p| [p[0], p[1], p[2], p[3]])
                .collect();
            let text_xs: Vec<i32> = (0..200)
                .filter(|&x| {
                    (0..200).any(|y| {
                        let p = &pixels[(y * 200 + x) as usize];
                        p[3] == 255 && p[2] > 200
                    })
                })
                .collect();
            let text_ys: Vec<i32> = (0..200)
                .filter(|&y| {
                    (0..200).any(|x| {
                        let p = &pixels[(y * 200 + x) as usize];
                        p[3] == 255 && p[2] > 200
                    })
                })
                .collect();
            let min_x = *text_xs.iter().min().unwrap();
            let min_y = *text_ys.iter().min().unwrap();
            let expect_left = name == "top-left" || name == "bottom-left";
            let expect_top = name == "top-left" || name == "top-right";
            assert_eq!(min_x < 100, expect_left, "wrong x placement for {name}");
            assert_eq!(min_y < 100, expect_top, "wrong y placement for {name}");
        }
    }
}
