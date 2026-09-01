use anyhow::{Context, Result, anyhow, bail};
use eframe::egui_glow::{self, glow};

use glow::HasContext as _;

const TEXTURE_COUNT: usize = 3;

struct PendingFrame {
    width: u32,
    height: u32,
    bytes: Vec<u8>,
}

pub struct YuyvRenderer {
    program: Option<glow::Program>,
    vertex_array: Option<glow::VertexArray>,
    textures: Vec<glow::Texture>,
    sampler: glow::UniformLocation,
    texture_size: Option<[u32; 2]>,
    current_texture: Option<usize>,
    next_texture: usize,
    pending: Option<PendingFrame>,
}

#[expect(unsafe_code)]
impl YuyvRenderer {
    pub fn new(gl: &glow::Context) -> Result<Self> {
        let shader_version = egui_glow::ShaderVersion::get(gl);
        if !shader_version.is_new_shader_interface() {
            bail!("YUYV preview requires modern OpenGL; found {shader_version:?}");
        }

        unsafe {
            let program = gl
                .create_program()
                .map_err(|err| anyhow!("failed to create YUYV shader program: {err}"))?;

            let shader_sources = [
                (
                    glow::VERTEX_SHADER,
                    r#"
                        const vec2 positions[3] = vec2[3](
                            vec2(-1.0, -1.0),
                            vec2( 3.0, -1.0),
                            vec2(-1.0,  3.0)
                        );
                        const vec2 texture_coordinates[3] = vec2[3](
                            vec2(0.0, 0.0),
                            vec2(2.0, 0.0),
                            vec2(0.0, 2.0)
                        );

                        out vec2 v_uv;

                        void main() {
                            v_uv = texture_coordinates[gl_VertexID];
                            gl_Position = vec4(positions[gl_VertexID], 0.0, 1.0);
                        }
                    "#,
                ),
                (
                    glow::FRAGMENT_SHADER,
                    r#"
                        precision highp float;
                        precision highp int;

                        uniform sampler2D u_yuyv;
                        in vec2 v_uv;
                        out vec4 out_color;

                        void main() {
                            ivec2 packed_size = textureSize(u_yuyv, 0);
                            int width = packed_size.x * 2;
                            int source_x = clamp(
                                int(floor(v_uv.x * float(width))),
                                0,
                                width - 1
                            );
                            int source_y = clamp(
                                int(floor((1.0 - v_uv.y) * float(packed_size.y))),
                                0,
                                packed_size.y - 1
                            );

                            vec4 yuyv = texelFetch(
                                u_yuyv,
                                ivec2(source_x / 2, source_y),
                                0
                            );
                            float y_code = (source_x & 1) == 0 ? yuyv.r : yuyv.b;

                            // The capture card supplies limited-range Rec. 709 YCbCr.
                            float y = 1.16438356 * (y_code - 16.0 / 255.0);
                            float cb = yuyv.g - 128.0 / 255.0;
                            float cr = yuyv.a - 128.0 / 255.0;
                            vec3 rgb = vec3(
                                y + 1.79274107 * cr,
                                y - 0.21324861 * cb - 0.53290933 * cr,
                                y + 2.11240179 * cb
                            );

                            out_color = vec4(clamp(rgb, 0.0, 1.0), 1.0);
                        }
                    "#,
                ),
            ];

            let mut shaders = Vec::with_capacity(shader_sources.len());
            for (shader_type, source) in shader_sources {
                let shader = gl
                    .create_shader(shader_type)
                    .map_err(|err| anyhow!("failed to create YUYV shader: {err}"))?;
                gl.shader_source(
                    shader,
                    &format!("{}\n{}", shader_version.version_declaration(), source),
                );
                gl.compile_shader(shader);
                if !gl.get_shader_compile_status(shader) {
                    let log = gl.get_shader_info_log(shader);
                    gl.delete_shader(shader);
                    for shader in shaders {
                        gl.delete_shader(shader);
                    }
                    gl.delete_program(program);
                    bail!("failed to compile YUYV shader: {log}");
                }
                gl.attach_shader(program, shader);
                shaders.push(shader);
            }

            gl.link_program(program);
            if !gl.get_program_link_status(program) {
                let log = gl.get_program_info_log(program);
                for shader in shaders {
                    gl.delete_shader(shader);
                }
                gl.delete_program(program);
                bail!("failed to link YUYV shader program: {log}");
            }

            for shader in shaders {
                gl.detach_shader(program, shader);
                gl.delete_shader(shader);
            }

            let vertex_array = gl
                .create_vertex_array()
                .map_err(|err| anyhow!("failed to create YUYV vertex array: {err}"))?;
            let sampler = gl
                .get_uniform_location(program, "u_yuyv")
                .ok_or_else(|| anyhow!("YUYV shader has no u_yuyv sampler"))?;

            let mut textures = Vec::with_capacity(TEXTURE_COUNT);
            for _ in 0..TEXTURE_COUNT {
                let texture = gl
                    .create_texture()
                    .map_err(|err| anyhow!("failed to create YUYV texture: {err}"))?;
                gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                gl.tex_parameter_i32(
                    glow::TEXTURE_2D,
                    glow::TEXTURE_MIN_FILTER,
                    glow::NEAREST as i32,
                );
                gl.tex_parameter_i32(
                    glow::TEXTURE_2D,
                    glow::TEXTURE_MAG_FILTER,
                    glow::NEAREST as i32,
                );
                gl.tex_parameter_i32(
                    glow::TEXTURE_2D,
                    glow::TEXTURE_WRAP_S,
                    glow::CLAMP_TO_EDGE as i32,
                );
                gl.tex_parameter_i32(
                    glow::TEXTURE_2D,
                    glow::TEXTURE_WRAP_T,
                    glow::CLAMP_TO_EDGE as i32,
                );
                textures.push(texture);
            }
            gl.bind_texture(glow::TEXTURE_2D, None);

            Ok(Self {
                program: Some(program),
                vertex_array: Some(vertex_array),
                textures,
                sampler,
                texture_size: None,
                current_texture: None,
                next_texture: 0,
                pending: None,
            })
        }
    }

    pub fn set_frame(&mut self, width: usize, height: usize, bytes: Vec<u8>) -> Result<()> {
        if width == 0 || height == 0 || !width.is_multiple_of(2) {
            bail!("invalid YUYV frame dimensions {width}x{height}");
        }

        let expected = width
            .checked_mul(height)
            .and_then(|pixels| pixels.checked_mul(2))
            .ok_or_else(|| anyhow!("YUYV frame dimensions overflow: {width}x{height}"))?;
        if bytes.len() != expected {
            bail!(
                "invalid YUYV frame length: expected {expected} bytes, got {}",
                bytes.len()
            );
        }

        self.pending = Some(PendingFrame {
            width: width.try_into().context("YUYV width exceeds u32")?,
            height: height.try_into().context("YUYV height exceeds u32")?,
            bytes,
        });
        Ok(())
    }

    pub fn paint(&mut self, gl: &glow::Context) {
        if let Some(frame) = self.pending.take() {
            self.upload(gl, &frame);
        }

        let (Some(program), Some(vertex_array), Some(texture_index)) =
            (self.program, self.vertex_array, self.current_texture)
        else {
            return;
        };

        unsafe {
            gl.use_program(Some(program));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.textures[texture_index]));
            gl.uniform_1_i32(Some(&self.sampler), 0);
            gl.bind_vertex_array(Some(vertex_array));
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
        }
    }

    pub fn destroy(&mut self, gl: &glow::Context) {
        unsafe {
            for texture in self.textures.drain(..) {
                gl.delete_texture(texture);
            }
            if let Some(vertex_array) = self.vertex_array.take() {
                gl.delete_vertex_array(vertex_array);
            }
            if let Some(program) = self.program.take() {
                gl.delete_program(program);
            }
        }
    }

    fn upload(&mut self, gl: &glow::Context, frame: &PendingFrame) {
        let size = [frame.width, frame.height];
        unsafe {
            gl.active_texture(glow::TEXTURE0);

            if self.texture_size != Some(size) {
                for &texture in &self.textures {
                    gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                    gl.tex_image_2d(
                        glow::TEXTURE_2D,
                        0,
                        glow::RGBA8 as i32,
                        (frame.width / 2) as i32,
                        frame.height as i32,
                        0,
                        glow::RGBA,
                        glow::UNSIGNED_BYTE,
                        glow::PixelUnpackData::Slice(None),
                    );
                }
                self.texture_size = Some(size);
                self.current_texture = None;
                self.next_texture = 0;
            }

            let texture_index = self.next_texture;
            gl.bind_texture(glow::TEXTURE_2D, Some(self.textures[texture_index]));
            gl.tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                0,
                0,
                (frame.width / 2) as i32,
                frame.height as i32,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(&frame.bytes)),
            );

            self.current_texture = Some(texture_index);
            self.next_texture = (texture_index + 1) % self.textures.len();
        }
    }
}
