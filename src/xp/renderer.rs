//! Cross-platform GPU renderer backed by wgpu.
//!
//! This module translates Maggie's GLES2 shaders and rendering pipeline to
//! wgpu, enabling rendering on Windows (DX12/Vulkan), macOS (Metal), and
//! Linux (Vulkan/GL) through a single code path.

use std::sync::Arc;

use wgpu::util::DeviceExt;

/// Uniform buffer for the frame sampling pass.
#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct FrameUniforms {
    src: [f32; 4], // (x, y, w, h) in texture space
}

/// Uniform buffer for the crosshair/overlay pass.
#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct SpriteUniforms {
    rect: [f32; 4], // (x, y, w, h) in normalized surface coords
    uv_offset: [f32; 2],
    _pad: [f32; 2],
}

/// Uniform for the crosshair cursor shader.
#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct CrosshairUniforms {
    size: f32,
    center: [f32; 2],
    _pad: f32,
}

/// GPU-accelerated renderer backed by wgpu.
pub struct XpRenderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    /// Bind group layout shared by all render pipelines.
    bind_group_layout: wgpu::BindGroupLayout,
    /// Pipeline for drawing the magnified frame.
    frame_pipeline: wgpu::RenderPipeline,
    /// Pipeline for drawing sprites (cursor, OSD, minimap).
    sprite_pipeline: wgpu::RenderPipeline,
    /// Pipeline for the annotation overlay with UV offset.
    overlay_pipeline: wgpu::RenderPipeline,
    /// Pipeline for the inverted-color crosshair cursor.
    crosshair_pipeline: wgpu::RenderPipeline,
    /// Texture for the captured frame.
    frame_texture: Option<wgpu::Texture>,
    /// Texture for OSD/sprite uploads.
    sprite_texture: Option<wgpu::Texture>,
    /// Texture for the overlay buffer.
    overlay_texture: Option<wgpu::Texture>,
    /// Sampler with nearest-neighbor filtering (crisp pixel magnifier).
    nearest_sampler: wgpu::Sampler,
    /// Sampler with linear filtering (smooth downscale).
    linear_sampler: wgpu::Sampler,
    /// Surface width in pixels.
    width: u32,
    /// Surface height in pixels.
    height: u32,
}

impl XpRenderer {
    /// Create a new wgpu renderer from a winit window.
    pub async fn new(window: Arc<winit::window::Window>) -> anyhow::Result<Self> {
        let size = window.inner_size();

        // Create wgpu instance with all backends.
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });

        // Create surface from the window.
        let surface = instance.create_surface(window)?;

        // Request adapter with high-performance preference.
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .ok_or_else(|| anyhow::anyhow!("No suitable GPU adapter found"))?;

        tracing::info!(
            "wgpu adapter: {} ({:?})",
            adapter.get_info().name,
            adapter.get_info().backend
        );

        // Create device and queue.
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("Maggie Device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_webgl2_defaults(),
                ..Default::default()
            }, None)
            .await?;

        // Configure surface.
        let surface_caps = surface.get_capabilities(&adapter);
        let surface_format = surface_caps
            .formats
            .iter()
            .find(|f| f.is_srgb())
            .copied()
            .unwrap_or(surface_caps.formats[0]);

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_DST,
            format: surface_format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            alpha_mode: surface_caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        // Create bind group layout (shared by all pipelines).
        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("Maggie BGL"),
                entries: &[
                    // Uniform buffer (slot 0)
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    // Texture (slot 1)
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    // Sampler (slot 2)
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Maggie Pipeline Layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        // WGSL shaders translated from the GLSL ES originals.

        // Frame vertex shader: samples u_src rect from the texture.
        let frame_vs = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Frame VS"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/frame.wgsl").into()),
        });

        // Sprite vertex shader: positions a sprite by u_rect in normalized coords.
        let sprite_vs = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Sprite VS"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/sprite.wgsl").into()),
        });

        // Frame fragment shader: samples texture, paints OOB black.
        let frame_fs = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Frame FS"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/frame_fs.wgsl").into()),
        });

        // Sprite fragment shader: samples texture with alpha blend.
        let sprite_fs = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Sprite FS"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/sprite_fs.wgsl").into()),
        });

        // Overlay fragment shader: UV-offset for pan-without-rerender.
        let overlay_fs = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Overlay FS"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/overlay_fs.wgsl").into()),
        });

        // Crosshair fragment shader: luminance crosshair for diff-blend inversion.
        let crosshair_fs = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Crosshair FS"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/crosshair_fs.wgsl").into()),
        });

        // Frame pipeline (nearest-neighbor frame sampling).
        let frame_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Frame Pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &frame_vs,
                entry_point: Some("vs_main"),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: 2 * 4, // 2 x f32
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &[wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32x2,
                        offset: 0,
                        shader_location: 0,
                    }],
                }],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &frame_fs,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        // Sprite pipeline (alpha-blended sprites).
        let sprite_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Sprite Pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &sprite_vs,
                entry_point: Some("vs_main"),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: 2 * 4,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &[wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32x2,
                        offset: 0,
                        shader_location: 0,
                    }],
                }],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &sprite_fs,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        // Overlay pipeline (alpha blend + UV offset).
        let overlay_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Overlay Pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &sprite_vs, // same vertex shader as sprite
                entry_point: Some("vs_main"),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: 2 * 4,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &[wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32x2,
                        offset: 0,
                        shader_location: 0,
                    }],
                }],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &overlay_fs,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        // Crosshair pipeline (diff-blend for inversion).
        let crosshair_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Crosshair Pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &sprite_vs,
                entry_point: Some("vs_main"),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: 2 * 4,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &[wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32x2,
                        offset: 0,
                        shader_location: 0,
                    }],
                }],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &crosshair_fs,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState {
                        color: wgpu::BlendComponent {
                    src_factor: wgpu::BlendFactor::OneMinusDst,
                    dst_factor: wgpu::BlendFactor::OneMinusSrc,
                            operation: wgpu::BlendOperation::Add,
                        },
                        alpha: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::One,
                            dst_factor: wgpu::BlendFactor::Zero,
                            operation: wgpu::BlendOperation::Add,
                        },
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let nearest_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let linear_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        Ok(Self {
            surface,
            device,
            queue,
            config,
            bind_group_layout,
            frame_pipeline,
            sprite_pipeline,
            overlay_pipeline,
            crosshair_pipeline,
            frame_texture: None,
            sprite_texture: None,
            overlay_texture: None,
            nearest_sampler,
            linear_sampler,
            width: size.width,
            height: size.height,
        })
    }

    /// Resize the surface. Call this when the window is resized.
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.width = width;
        self.height = height;
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
    }

    /// Upload the captured frame as a texture.
    pub fn upload_frame(&mut self, rgba: &[u8], frame_width: u32, frame_height: u32) {
        Self::get_or_create_texture(
            &self.device,
            &mut self.frame_texture,
            "frame",
            frame_width,
            frame_height,
        );
        let tex = self.frame_texture.as_ref().unwrap();
        self.queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            rgba,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(4 * frame_width),
                rows_per_image: Some(frame_height),
            },
            wgpu::Extent3d {
                width: frame_width,
                height: frame_height,
                depth_or_array_layers: 1,
            },
        );
    }

    /// Render a frame with the magnified view, optional OSD, cursor, etc.
    pub fn render(
        &mut self,
        src_uv: Option<[f32; 4]>,
        frame_width: u32,
        frame_height: u32,
        overlay: Option<(&[u8], u32, u32)>,
        cursor_sprite: Option<(&[u8], u32, u32, [f32; 2])>,
        minimap_sprite: Option<(&[u8], u32, u32, [f32; 2])>,
        osd_sprite: Option<(&[u8], u32, u32, [f32; 2])>,
    ) {
        let output = match self.surface.get_current_texture() {
            Ok(t) => t,
            Err(_) => {
                self.surface.configure(&self.device, &self.config);
                return;
            }
        };
        let view = output.texture.create_view(&Default::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("Maggie Encoder"),
            });

        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Maggie Render Pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                ..Default::default()
            });

            // Draw the magnified frame.
            if let (Some(frame_tex), Some(src)) =
                (&self.frame_texture, src_uv)
            {
                let uniform_buf = self
                    .device
                    .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("Frame Uniforms"),
                        contents: bytemuck::bytes_of(&FrameUniforms { src }),
                        usage: wgpu::BufferUsages::UNIFORM,
                    });

                let frame_view = frame_tex.create_view(&Default::default());
                let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("Frame BG"),
                    layout: &self.bind_group_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: uniform_buf.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(&frame_view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::Sampler(&self.nearest_sampler),
                        },
                    ],
                });

                render_pass.set_pipeline(&self.frame_pipeline);
                render_pass.set_bind_group(0, &bind_group, &[]);
                render_pass.set_vertex_buffer(0, self.quad_vbo().slice(..));
                render_pass.draw(0..6, 0..1);
            }

            // Draw screenshot/annotation overlay (alpha-blended fullscreen sprite).
            if let Some((data, w, h)) = overlay {
                self.draw_sprite(
                    &mut render_pass,
                    data, w, h, [0.0, 0.0],
                    &self.overlay_pipeline,
                    "overlay",
                );
            }

            // Draw cursor sprite.
            if let Some((data, w, h, pos)) = cursor_sprite {
                self.draw_sprite(
                    &mut render_pass,
                    data, w, h, pos,
                    &self.sprite_pipeline,
                    "cursor",
                );
            }

            // Draw minimap sprite (bottom-right corner).
            if let Some((data, w, h, pos)) = minimap_sprite {
                self.draw_sprite(
                    &mut render_pass,
                    data, w, h, pos,
                    &self.sprite_pipeline,
                    "minimap",
                );
            }

            // Draw OSD sprite.
            if let Some((data, w, h, pos)) = osd_sprite {
                self.draw_sprite(
                    &mut render_pass,
                    data, w, h, pos,
                    &self.sprite_pipeline,
                    "osd",
                );
            }
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        output.present();
    }

    /// Draw a sprite texture at a given position.
    fn draw_sprite(
        &self,
        render_pass: &mut wgpu::RenderPass<'_>,
        data: &[u8],
        w: u32,
        h: u32,
        pos: [f32; 2],
        pipeline: &wgpu::RenderPipeline,
        label: &str,
    ) {
        let tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        self.queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(4 * w),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );

        let surf_w = self.width as f32;
        let surf_h = self.height as f32;
        let rect = [
            pos[0] / surf_w,
            pos[1] / surf_h,
            w as f32 / surf_w,
            h as f32 / surf_h,
        ];

        let uniform_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Sprite Uniforms"),
            contents: bytemuck::bytes_of(&SpriteUniforms {
                rect,
                uv_offset: [0.0, 0.0],
                _pad: [0.0, 0.0],
            }),
            usage: wgpu::BufferUsages::UNIFORM,
        });

        let tex_view = tex.create_view(&Default::default());
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Sprite BG"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&tex_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.linear_sampler),
                },
            ],
        });

        render_pass.set_pipeline(pipeline);
        render_pass.set_bind_group(0, &bind_group, &[]);
        render_pass.set_vertex_buffer(0, self.quad_vbo().slice(..));
        render_pass.draw(0..6, 0..1);
    }

    /// Get the wgpu device (for egui integration).
    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    /// Get the surface format.
    pub fn surface_format(&self) -> wgpu::TextureFormat {
        self.config.format
    }

    fn get_or_create_texture(
        device: &wgpu::Device,
        slot: &mut Option<wgpu::Texture>,
        label: &str,
        width: u32,
        height: u32,
    ) {
        let needs_create = match slot {
            Some(tex) => tex.width() != width || tex.height() != height,
            None => true,
        };
        if needs_create {
            *slot = Some(device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            }));
        }
    }

    /// Create a static vertex buffer for a fullscreen quad.
    /// Called once; cached as a return value (the vertices are inline).
    fn quad_vbo(&self) -> wgpu::Buffer {
        // Fullscreen quad: two triangles covering [0,1] x [0,1].
        #[rustfmt::skip]
        let verts: [[f32; 2]; 6] = [
            [0.0, 0.0], [1.0, 0.0], [1.0, 1.0],
            [0.0, 0.0], [1.0, 1.0], [0.0, 1.0],
        ];
        self.device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("Quad VBO"),
                contents: bytemuck::cast_slice(&verts),
                usage: wgpu::BufferUsages::VERTEX,
            })
    }
}
