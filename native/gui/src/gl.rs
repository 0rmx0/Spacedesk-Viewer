//! OpenGL renderer (via `glow`).
//!
//! The remote screen is kept as an offscreen "canvas" texture (FBO). Each
//! decoded tile (a dirty rect of the screen) is uploaded to a tile texture and
//! drawn into the canvas at its rect with a textured quad — compositing happens
//! entirely on the GPU. Presenting = one fullscreen triangle sampling the canvas,
//! plus an alpha-blended quad for the remote cursor.

use glow::HasContext;
use spacedesk_core::frame::{DecodedFrame, PixelFormat};

const PRESENT_VS: &str = r#"#version 330 core
out vec2 v_uv;
void main() {
    vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
    v_uv = vec2(p.x, 1.0 - p.y);
    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}
"#;

const QUAD_VS: &str = r#"#version 330 core
layout(location = 0) in vec2 a_pos;   // unit quad
layout(location = 1) in vec2 a_uv;
uniform vec2 u_resolution;             // target size in pixels
uniform vec2 u_offset;                 // dest rect origin in pixels (top-left)
uniform vec2 u_size;                   // dest rect size in pixels
out vec2 v_uv;
void main() {
    vec2 pos = u_offset + a_pos * u_size;
    vec2 clip = vec2(pos.x / u_resolution.x * 2.0 - 1.0,
                     1.0 - pos.y / u_resolution.y * 2.0);
    v_uv = a_uv;
    gl_Position = vec4(clip, 0.0, 1.0);
}
"#;

const TEX_FS: &str = r#"#version 330 core
in vec2 v_uv;
out vec4 frag;
uniform sampler2D u_tex;
void main() {
    frag = texture(u_tex, v_uv);
}
"#;

pub struct CursorImage {
    pub width: u32,
    pub height: u32,
    pub x_hot: i32,
    pub y_hot: i32,
    pub rgba: Vec<u8>,
}

pub struct Renderer {
    gl: glow::Context,
    present_prog: glow::Program,
    quad_prog: glow::Program,
    canvas_tex: glow::Texture,
    canvas_fbo: glow::Framebuffer,
    canvas_size: (u32, u32),
    tile_tex: glow::Texture,
    tile_size: (u32, u32),
    cursor_tex: glow::Texture,
    cursor_size: (u32, u32),
    quad_vao: glow::VertexArray,
    _quad_vbo: glow::Buffer,
}

impl Renderer {
    pub unsafe fn new(gl: glow::Context) -> Renderer {
        let present_prog = link(&gl, PRESENT_VS, TEX_FS);
        let quad_prog = link(&gl, QUAD_VS, TEX_FS);

        let mk_tex = |gl: &glow::Context| {
            let t = gl.create_texture().unwrap();
            gl.bind_texture(glow::TEXTURE_2D, Some(t));
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
            t
        };
        let canvas_tex = mk_tex(&gl);
        let tile_tex = mk_tex(&gl);
        let cursor_tex = mk_tex(&gl);
        let canvas_fbo = gl.create_framebuffer().unwrap();

        // unit quad (two triangles)
        let quad_vao = gl.create_vertex_array().unwrap();
        gl.bind_vertex_array(Some(quad_vao));
        let quad_vbo = gl.create_buffer().unwrap();
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(quad_vbo));
        #[rustfmt::skip]
        let quad: [f32; 24] = [
            0.0, 0.0,  0.0, 0.0,
            1.0, 0.0,  1.0, 0.0,
            1.0, 1.0,  1.0, 1.0,
            0.0, 0.0,  0.0, 0.0,
            1.0, 1.0,  1.0, 1.0,
            0.0, 1.0,  0.0, 1.0,
        ];
        gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, f32_slice_as_u8(&quad), glow::STATIC_DRAW);
        gl.enable_vertex_attrib_array(0);
        gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 16, 0);
        gl.enable_vertex_attrib_array(1);
        gl.vertex_attrib_pointer_f32(1, 2, glow::FLOAT, false, 16, 8);
        gl.bind_vertex_array(None);

        gl.disable(glow::DEPTH_TEST);
        gl.disable(glow::CULL_FACE);

        Renderer {
            gl,
            present_prog,
            quad_prog,
            canvas_tex,
            canvas_fbo,
            canvas_size: (0, 0),
            tile_tex,
            tile_size: (0, 0),
            cursor_tex,
            cursor_size: (0, 0),
            quad_vao,
            _quad_vbo: quad_vbo,
        }
    }

    unsafe fn ensure_canvas(&mut self, w: u32, h: u32) {
        if self.canvas_size == (w, h) {
            return;
        }
        let gl = &self.gl;
        gl.bind_texture(glow::TEXTURE_2D, Some(self.canvas_tex));
        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
        gl.tex_image_2d(
            glow::TEXTURE_2D, 0, glow::RGBA as i32, w as i32, h as i32, 0,
            glow::RGBA, glow::UNSIGNED_BYTE, None,
        );
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.canvas_fbo));
        gl.framebuffer_texture_2d(
            glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(self.canvas_tex), 0,
        );
        gl.viewport(0, 0, w as i32, h as i32);
        gl.clear_color(0.0, 0.0, 0.0, 1.0);
        gl.clear(glow::COLOR_BUFFER_BIT);
        gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        self.canvas_size = (w, h);
    }

    /// Composite one decoded tile into the canvas texture at its rect.
    pub unsafe fn composite_tile(&mut self, frame: &DecodedFrame, screen: (u32, u32)) {
        self.ensure_canvas(screen.0.max(1), screen.1.max(1));
        let gl = &self.gl;
        let (tw, th) = (frame.width.max(1), frame.height.max(1));

        if self.tile_size != (tw, th) {
            gl.bind_texture(glow::TEXTURE_2D, Some(self.tile_tex));
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
            gl.tex_image_2d(
                glow::TEXTURE_2D, 0, glow::RGBA as i32, tw as i32, th as i32, 0,
                glow::RGBA, glow::UNSIGNED_BYTE, None,
            );
            self.tile_size = (tw, th);
        }
        let (fmt, align) = match frame.format {
            PixelFormat::Bgra => (glow::BGRA, 4),
            PixelFormat::Rgba => (glow::RGBA, 4),
            PixelFormat::Nv12 => (glow::RGBA, 4), // not produced by current decoders
        };
        gl.bind_texture(glow::TEXTURE_2D, Some(self.tile_tex));
        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, align);
        gl.tex_sub_image_2d(
            glow::TEXTURE_2D, 0, 0, 0, tw as i32, th as i32,
            fmt, glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(&frame.data),
        );

        // draw the tile into the canvas FBO at its rect
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.canvas_fbo));
        gl.viewport(0, 0, screen.0.max(1) as i32, screen.1.max(1) as i32);
        gl.use_program(Some(self.quad_prog));
        gl.uniform_2_f32(gl.get_uniform_location(self.quad_prog, "u_resolution").as_ref(), screen.0 as f32, screen.1 as f32);
        gl.uniform_2_f32(gl.get_uniform_location(self.quad_prog, "u_offset").as_ref(), frame.rect.x as f32, frame.rect.y as f32);
        gl.uniform_2_f32(gl.get_uniform_location(self.quad_prog, "u_size").as_ref(), tw as f32, th as f32);
        gl.active_texture(glow::TEXTURE0);
        gl.bind_texture(glow::TEXTURE_2D, Some(self.tile_tex));
        gl.uniform_1_i32(gl.get_uniform_location(self.quad_prog, "u_tex").as_ref(), 0);
        gl.bind_vertex_array(Some(self.quad_vao));
        gl.draw_arrays(glow::TRIANGLES, 0, 6);
        gl.bind_vertex_array(None);
        gl.bind_framebuffer(glow::FRAMEBUFFER, None);
    }

    /// Draw the canvas to the window, plus the remote cursor if any.
    pub unsafe fn present(&mut self, viewport: (u32, u32), cursor: Option<(&CursorImage, i32, i32)>) {
        let gl = &self.gl;
        gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        gl.viewport(0, 0, viewport.0 as i32, viewport.1 as i32);
        gl.clear_color(0.0, 0.0, 0.0, 1.0);
        gl.clear(glow::COLOR_BUFFER_BIT);

        if self.canvas_size != (0, 0) {
            gl.use_program(Some(self.present_prog));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.canvas_tex));
            gl.uniform_1_i32(gl.get_uniform_location(self.present_prog, "u_tex").as_ref(), 0);
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
        }

        if let Some((cur, cx, cy)) = cursor {
            if self.cursor_size != (cur.width, cur.height) {
                gl.bind_texture(glow::TEXTURE_2D, Some(self.cursor_tex));
                gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
                gl.tex_image_2d(
                    glow::TEXTURE_2D, 0, glow::RGBA as i32, cur.width as i32, cur.height as i32, 0,
                    glow::RGBA, glow::UNSIGNED_BYTE, Some(&cur.rgba),
                );
                self.cursor_size = (cur.width, cur.height);
            }
            gl.enable(glow::BLEND);
            gl.blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);
            gl.use_program(Some(self.quad_prog));
            gl.uniform_2_f32(gl.get_uniform_location(self.quad_prog, "u_resolution").as_ref(), viewport.0 as f32, viewport.1 as f32);
            gl.uniform_2_f32(gl.get_uniform_location(self.quad_prog, "u_offset").as_ref(), (cx - cur.x_hot) as f32, (cy - cur.y_hot) as f32);
            gl.uniform_2_f32(gl.get_uniform_location(self.quad_prog, "u_size").as_ref(), cur.width as f32, cur.height as f32);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.cursor_tex));
            gl.uniform_1_i32(gl.get_uniform_location(self.quad_prog, "u_tex").as_ref(), 0);
            gl.bind_vertex_array(Some(self.quad_vao));
            gl.draw_arrays(glow::TRIANGLES, 0, 6);
            gl.bind_vertex_array(None);
            gl.disable(glow::BLEND);
        }
    }

    pub unsafe fn clear_screen(&self) {
        let gl = &self.gl;
        gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        gl.clear_color(0.0, 0.0, 0.0, 1.0);
        gl.clear(glow::COLOR_BUFFER_BIT);
    }
}

unsafe fn link(gl: &glow::Context, vs_src: &str, fs_src: &str) -> glow::Program {
    let prog = gl.create_program().unwrap();
    let vs = compile(gl, glow::VERTEX_SHADER, vs_src);
    let fs = compile(gl, glow::FRAGMENT_SHADER, fs_src);
    gl.attach_shader(prog, vs);
    gl.attach_shader(prog, fs);
    gl.link_program(prog);
    if !gl.get_program_link_status(prog) {
        panic!("link error: {}", gl.get_program_info_log(prog));
    }
    gl.delete_shader(vs);
    gl.delete_shader(fs);
    prog
}

unsafe fn compile(gl: &glow::Context, kind: u32, src: &str) -> glow::Shader {
    let sh = gl.create_shader(kind).unwrap();
    gl.shader_source(sh, src);
    gl.compile_shader(sh);
    if !gl.get_shader_compile_status(sh) {
        panic!("shader error: {}", gl.get_shader_info_log(sh));
    }
    sh
}

fn f32_slice_as_u8(slice: &[f32]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(slice.as_ptr() as *const u8, slice.len() * 4) }
}

/// Convert a spacedesk cursor bitmap payload to RGBA.
/// flags: 1 = monochrome (1bpp AND/XOR mask), 4 = masked color (BGRA pixels).
pub fn cursor_payload_to_rgba(width: u32, height: u32, pitch: u32, flags: u32, payload: &[u8]) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let mut out = vec![0u8; w * h * 4];
    match flags {
        4 => {
            let pitch = if pitch == 0 { w * 4 } else { pitch as usize };
            for y in 0..h {
                for x in 0..w {
                    let src = y * pitch + x * 4;
                    if src + 4 <= payload.len() {
                        let (b, g, r, a) = (payload[src], payload[src + 1], payload[src + 2], payload[src + 3]);
                        let dst = (y * w + x) * 4;
                        out[dst] = r;
                        out[dst + 1] = g;
                        out[dst + 2] = b;
                        out[dst + 3] = if a == 0 { 0 } else { 255 };
                    }
                }
            }
        }
        1 => {
            let row_bytes = ((w + 31) / 32) * 4;
            let and_len = row_bytes * h;
            for y in 0..h {
                for x in 0..w {
                    let byte = y * row_bytes + x / 8;
                    let bit = 7 - (x % 8);
                    let and = payload.get(byte).map(|b| (b >> bit) & 1).unwrap_or(1);
                    let xor = payload.get(and_len + byte).map(|b| (b >> bit) & 1).unwrap_or(0);
                    let dst = (y * w + x) * 4;
                    let v = if and == 0 { if xor == 1 { 255 } else { 0 } } else { 0 };
                    out[dst] = v;
                    out[dst + 1] = v;
                    out[dst + 2] = v;
                    out[dst + 3] = if and == 0 { 255 } else { 0 };
                }
            }
        }
        _ => {}
    }
    out
}
