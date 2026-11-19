//! Minimal OpenGL renderer (via `glow`): one fullscreen triangle for the video
//! frame (NV12 via 2 textures with YUV->RGB in the fragment shader, or a single
//! RGBA/BGRA texture), plus an alpha-blended quad for the remote cursor.

use glow::HasContext;
use spacedesk_core::frame::{DecodedFrame, PixelFormat};

const VIDEO_VS: &str = r#"#version 330 core
out vec2 v_uv;
void main() {
    vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
    v_uv = vec2(p.x, 1.0 - p.y);
    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}
"#;

const VIDEO_FS: &str = r#"#version 330 core
in vec2 v_uv;
out vec4 frag;
uniform sampler2D u_tex;
uniform sampler2D u_tex_uv;
uniform int u_mode; // 0 = single RGBA/BGRA texture, 1 = NV12
void main() {
    if (u_mode == 1) {
        float y = texture(u_tex, v_uv).r;
        vec2 uv = texture(u_tex_uv, v_uv).rg - 0.5;
        frag = vec4(y + 1.402 * uv.y,
                    y - 0.344136 * uv.x - 0.714136 * uv.y,
                    y + 1.772 * uv.x, 1.0);
    } else {
        frag = texture(u_tex, v_uv);
    }
}
"#;

const CURSOR_VS: &str = r#"#version 330 core
layout(location = 0) in vec2 a_pos;   // pixel coords (top-left origin)
layout(location = 1) in vec2 a_uv;
uniform vec2 u_resolution;
uniform vec2 u_offset;
uniform vec2 u_size;
out vec2 v_uv;
void main() {
    vec2 pos = u_offset + a_pos * u_size;
    vec2 clip = vec2(pos.x / u_resolution.x * 2.0 - 1.0,
                     1.0 - pos.y / u_resolution.y * 2.0);
    v_uv = a_uv;
    gl_Position = vec4(clip, 0.0, 1.0);
}
"#;

const CURSOR_FS: &str = r#"#version 330 core
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
    video_prog: glow::Program,
    cursor_prog: glow::Program,
    tex_main: glow::Texture,
    tex_uv: glow::Texture,
    cursor_tex: glow::Texture,
    _cursor_vbo: glow::Buffer,
    cursor_vao: glow::VertexArray,
    main_size: (u32, u32),
    uv_size: (u32, u32),
    cursor_size: (u32, u32),
}

impl Renderer {
    pub unsafe fn new(gl: glow::Context) -> Renderer {
        let video_prog = link(&gl, VIDEO_VS, VIDEO_FS);
        let cursor_prog = link(&gl, CURSOR_VS, CURSOR_FS);

        let tex_main = gl.create_texture().unwrap();
        gl.bind_texture(glow::TEXTURE_2D, Some(tex_main));
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);

        let tex_uv = gl.create_texture().unwrap();
        gl.bind_texture(glow::TEXTURE_2D, Some(tex_uv));
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);

        let cursor_tex = gl.create_texture().unwrap();
        gl.bind_texture(glow::TEXTURE_2D, Some(cursor_tex));
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);

        // unit quad (two triangles) for the cursor
        let cursor_vao = gl.create_vertex_array().unwrap();
        gl.bind_vertex_array(Some(cursor_vao));
        let cursor_vbo = gl.create_buffer().unwrap();
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(cursor_vbo));
        #[rustfmt::skip]
        let quad: [f32; 24] = [
            // pos      // uv
            0.0, 0.0,    0.0, 0.0,
            1.0, 0.0,    1.0, 0.0,
            1.0, 1.0,    1.0, 1.0,
            0.0, 0.0,    0.0, 0.0,
            1.0, 1.0,    1.0, 1.0,
            0.0, 1.0,    0.0, 1.0,
        ];
        gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytemuck_cast(&quad), glow::STATIC_DRAW);
        gl.enable_vertex_attrib_array(0);
        gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 16, 0);
        gl.enable_vertex_attrib_array(1);
        gl.vertex_attrib_pointer_f32(1, 2, glow::FLOAT, false, 16, 8);
        gl.bind_vertex_array(None);

        gl.disable(glow::DEPTH_TEST);
        gl.disable(glow::CULL_FACE);

        Renderer {
            gl,
            video_prog,
            cursor_prog,
            tex_main,
            tex_uv,
            cursor_tex,
            _cursor_vbo: cursor_vbo,
            cursor_vao,
            main_size: (0, 0),
            uv_size: (0, 0),
            cursor_size: (0, 0),
        }
    }

    pub unsafe fn clear(&self) {
        self.gl.clear_color(0.0, 0.0, 0.0, 1.0);
        self.gl.clear(glow::COLOR_BUFFER_BIT);
    }

    pub unsafe fn resize(&self, width: u32, height: u32) {
        self.gl.viewport(0, 0, width as i32, height as i32);
    }

    unsafe fn alloc_main(gl: &glow::Context, tex: glow::Texture, w: u32, h: u32, format: u32, type_: u32, internal: i32) {
        gl.bind_texture(glow::TEXTURE_2D, Some(tex));
        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            internal,
            w as i32,
            h as i32,
            0,
            format,
            type_,
            None,
        );
    }

    /// Upload `frame` and draw it stretched to the whole viewport, then the
    /// remote cursor if provided.
    pub unsafe fn draw(
        &mut self,
        frame: &DecodedFrame,
        viewport: (u32, u32),
        cursor: Option<(&CursorImage, i32, i32)>,
    ) {
                self.gl.viewport(0, 0, viewport.0 as i32, viewport.1 as i32);
        self.gl.clear_color(0.0, 0.0, 0.0, 1.0);
        self.gl.clear(glow::COLOR_BUFFER_BIT);

        self.gl.use_program(Some(self.video_prog));
        match frame.format {
            PixelFormat::Nv12 => {
                let (w, h) = (frame.width, frame.height);
                let cw = (w + 1) / 2;
                let ch = (h + 1) / 2;
                if self.main_size != (w, h) {
                    Self::alloc_main(&self.gl, self.tex_main, w, h, glow::RED, glow::UNSIGNED_BYTE, glow::R8 as i32);
                    self.main_size = (w, h);
                }
                self.gl.active_texture(glow::TEXTURE0);
                self.gl.bind_texture(glow::TEXTURE_2D, Some(self.tex_main));
                self.gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
                self.gl.tex_sub_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    0,
                    0,
                    w as i32,
                    h as i32,
                    glow::RED,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(&frame.data[..(w * h) as usize]),
                );
                if self.uv_size != (cw, ch) {
                    self.gl.bind_texture(glow::TEXTURE_2D, Some(self.tex_uv));
                    self.gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
                    self.gl.tex_image_2d(
                        glow::TEXTURE_2D,
                        0,
                        glow::RG8 as i32,
                        cw as i32,
                        ch as i32,
                        0,
                        glow::RG,
                        glow::UNSIGNED_BYTE,
                        None,
                    );
                    self.uv_size = (cw, ch);
                }
                self.gl.active_texture(glow::TEXTURE1);
                self.gl.bind_texture(glow::TEXTURE_2D, Some(self.tex_uv));
                self.gl.tex_sub_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    0,
                    0,
                    cw as i32,
                    ch as i32,
                    glow::RG,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(&frame.data[(w * h) as usize..]),
                );
                self.gl.uniform_1_i32(
                    self.gl.get_uniform_location(self.video_prog, "u_tex").as_ref(),
                    0,
                );
                self.gl.uniform_1_i32(
                    self.gl.get_uniform_location(self.video_prog, "u_tex_uv").as_ref(),
                    1,
                );
                self.gl.uniform_1_i32(
                    self.gl.get_uniform_location(self.video_prog, "u_mode").as_ref(),
                    1,
                );
            }
            PixelFormat::Bgra => {
                if self.main_size != (frame.width, frame.height) {
                    Self::alloc_main(&self.gl, self.tex_main, frame.width, frame.height, glow::BGRA, glow::UNSIGNED_BYTE, glow::RGBA as i32);
                    self.main_size = (frame.width, frame.height);
                }
                self.gl.active_texture(glow::TEXTURE0);
                self.gl.bind_texture(glow::TEXTURE_2D, Some(self.tex_main));
                self.gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
                self.gl.tex_sub_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    0,
                    0,
                    frame.width as i32,
                    frame.height as i32,
                    glow::BGRA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(&frame.data),
                );
                self.gl.uniform_1_i32(
                    self.gl.get_uniform_location(self.video_prog, "u_tex").as_ref(),
                    0,
                );
                self.gl.uniform_1_i32(
                    self.gl.get_uniform_location(self.video_prog, "u_mode").as_ref(),
                    0,
                );
            }
            PixelFormat::Rgba => {
                if self.main_size != (frame.width, frame.height) {
                    Self::alloc_main(&self.gl, self.tex_main, frame.width, frame.height, glow::RGBA, glow::UNSIGNED_BYTE, glow::RGBA as i32);
                    self.main_size = (frame.width, frame.height);
                }
                self.gl.active_texture(glow::TEXTURE0);
                self.gl.bind_texture(glow::TEXTURE_2D, Some(self.tex_main));
                self.gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
                self.gl.tex_sub_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    0,
                    0,
                    frame.width as i32,
                    frame.height as i32,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(&frame.data),
                );
                self.gl.uniform_1_i32(
                    self.gl.get_uniform_location(self.video_prog, "u_tex").as_ref(),
                    0,
                );
                self.gl.uniform_1_i32(
                    self.gl.get_uniform_location(self.video_prog, "u_mode").as_ref(),
                    0,
                );
            }
        }
        self.gl.draw_arrays(glow::TRIANGLES, 0, 3);

        if let Some((cur, cx, cy)) = cursor {
            if self.cursor_size != (cur.width, cur.height) {
                self.gl.bind_texture(glow::TEXTURE_2D, Some(self.cursor_tex));
                self.gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
                self.gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::RGBA as i32,
                    cur.width as i32,
                    cur.height as i32,
                    0,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    Some(&cur.rgba),
                );
                self.cursor_size = (cur.width, cur.height);
            }
            self.gl.enable(glow::BLEND);
            self.gl.blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);
            self.gl.use_program(Some(self.cursor_prog));
            self.gl.uniform_2_f32(
                self.gl.get_uniform_location(self.cursor_prog, "u_resolution").as_ref(),
                viewport.0 as f32,
                viewport.1 as f32,
            );
            self.gl.uniform_2_f32(
                self.gl.get_uniform_location(self.cursor_prog, "u_offset").as_ref(),
                (cx - cur.x_hot) as f32,
                (cy - cur.y_hot) as f32,
            );
            self.gl.uniform_2_f32(
                self.gl.get_uniform_location(self.cursor_prog, "u_size").as_ref(),
                cur.width as f32,
                cur.height as f32,
            );
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(self.cursor_tex));
            self.gl.uniform_1_i32(
                self.gl.get_uniform_location(self.cursor_prog, "u_tex").as_ref(),
                0,
            );
            self.gl.bind_vertex_array(Some(self.cursor_vao));
            self.gl.draw_arrays(glow::TRIANGLES, 0, 6);
            self.gl.bind_vertex_array(None);
            self.gl.disable(glow::BLEND);
        }
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

fn bytemuck_cast(slice: &[f32]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(slice.as_ptr() as *const u8, slice.len() * 4) }
}

/// Convert a spacedesk cursor bitmap payload to RGBA.
/// flags: 1 = monochrome (1bpp AND/XOR mask), 2 = default, 4 = masked color (BGRA).
pub fn cursor_payload_to_rgba(width: u32, height: u32, pitch: u32, flags: u32, payload: &[u8]) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let mut out = vec![0u8; w * h * 4];
    match flags {
        4 => {
            // masked color: BGRA pixels
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
            // monochrome: 1bpp mask rows (AND mask then XOR mask), 32-bit aligned rows
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
        _ => {
            // unknown: leave transparent
        }
    }
    out
}
