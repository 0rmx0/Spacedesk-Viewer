//! Bitmap font atlas built at startup with `ab_glyph` from a system TTF
//! (DejaVu/Liberation). ASCII 0x20..=0x7E are rasterized once into an RGBA
//! atlas (white RGB, coverage in alpha) that the GL renderer samples.

use ab_glyph::{Font as _, FontArc, GlyphId, PxScale, ScaleFont as _};

pub const FIRST_CHAR: u32 = 0x20;
pub const GLYPH_COUNT: usize = 96; // 0x20..=0x7E
pub const ATLAS_COLS: u32 = 16;
pub const CELL_W: u32 = 24;
pub const CELL_H: u32 = 32;
pub const PIXEL_SIZE: f32 = 20.0;

#[derive(Clone, Copy, Default)]
pub struct Glyph {
    /// atlas UVs
    pub u0: f32,
    pub v0: f32,
    pub u1: f32,
    pub v1: f32,
    /// size in pixels
    pub w: f32,
    pub h: f32,
    /// offset from the pen position (x right, y up from baseline)
    pub xoff: f32,
    pub yoff: f32,
    pub advance: f32,
}

#[derive(Clone)]
pub struct FontAtlas {
    pub data: Vec<u8>, // RGBA
    pub width: u32,
    pub height: u32,
    pub glyphs: [Glyph; GLYPH_COUNT],
    pub ascent: f32,
}

fn load_font_bytes() -> Option<Vec<u8>> {
    let candidates = [
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        "/usr/share/fonts/truetype/freefont/FreeSans.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
    ];
    for p in candidates {
        if let Ok(b) = std::fs::read(p) {
            return Some(b);
        }
    }
    None
}

pub fn build_atlas() -> Option<FontAtlas> {
    let bytes = load_font_bytes()?;
    let font = FontArc::try_from_vec(bytes).ok()?;
    let scale = PxScale::from(PIXEL_SIZE);
    let scaled = font.as_scaled(scale);
    let atlas_w = ATLAS_COLS * CELL_W;
    let atlas_h = (GLYPH_COUNT as u32).div_ceil(ATLAS_COLS) * CELL_H;
    let mut data = vec![0u8; (atlas_w * atlas_h * 4) as usize];
    let mut glyphs = [Glyph::default(); GLYPH_COUNT];
    let ascent = scaled.ascent();
    for (i, slot) in glyphs.iter_mut().enumerate() {
        let c = char::from_u32(FIRST_CHAR + i as u32)?;
        let gid: GlyphId = font.glyph_id(c);
        let cell_x = (i as u32 % ATLAS_COLS) * CELL_W;
        let cell_y = (i as u32 / ATLAS_COLS) * CELL_H;
        // pen position inside the cell (y-up space, baseline at cell_y + ascent)
        let pen_x = cell_x as f32;
        let pen_y = cell_y as f32 + ascent;
        let glyph = gid.with_scale_and_position(scale, ab_glyph::point(pen_x, pen_y));
        let advance = scaled.h_advance(gid);
        if let Some(outlined) = font.outline_glyph(glyph) {
            let bb = outlined.px_bounds();
            let (w, h) = (bb.width(), bb.height());
            if w > 0.0 && h > 0.0 {
                // px_bounds is absolute (pen position included), y-down:
                // the glyph bitmap occupies [bb.min, bb.max) in the atlas.
                let ox = bb.min.x;
                let oy = bb.min.y;
                outlined.draw(|x, y, v| {
                    let px = (ox as i32 + x as i32) as u32;
                    let py = (oy as i32 + y as i32) as u32;
                    if px < atlas_w && py < atlas_h {
                        let o = ((py * atlas_w + px) * 4) as usize;
                        let a = (v.clamp(0.0, 1.0) * 255.0) as u8;
                        data[o] = 255;
                        data[o + 1] = 255;
                        data[o + 2] = 255;
                        data[o + 3] = a;
                    }
                });
                *slot = Glyph {
                    u0: ox / atlas_w as f32,
                    v0: oy / atlas_h as f32,
                    u1: (ox + w) / atlas_w as f32,
                    v1: (oy + h) / atlas_h as f32,
                    w,
                    h,
                    xoff: bb.min.x - pen_x,
                    yoff: pen_y - bb.min.y, // distance above baseline
                    advance,
                };
            } else {
                *slot = Glyph {
                    advance,
                    ..Default::default()
                };
            }
        } else {
            *slot = Glyph {
                advance,
                ..Default::default()
            };
        }
    }
    Some(FontAtlas {
        data,
        width: atlas_w,
        height: atlas_h,
        glyphs,
        ascent,
    })
}
