use std::{net::SocketAddr, sync::Arc};

use anyhow::Result;
use protocol::{
    CHUNK_AREA, CHUNK_EDGE, Cell, Chunk, ChunkCoord, ClientMessage, Direction, Occupant,
    STEM_CONNECT_EAST, STEM_CONNECT_NORTH, STEM_CONNECT_SOUTH, STEM_CONNECT_WEST,
};
use tokio::sync::mpsc::UnboundedSender;
use tracing::info;
use wgpu::util::DeviceExt;
use winit::{event::WindowEvent, window::Window};

use crate::app::{Camera, ContextMenu, NetworkStatus};

const MSAA_SAMPLES: u32 = 4;

pub const LAYER_ORGANIC: u32 = 1 << 0;
pub const LAYER_FG: u32 = 1 << 1;
pub const LAYER_ENERGY: u32 = 1 << 2;

pub struct RenderState {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    msaa_view: wgpu::TextureView,
    egui_ctx: egui::Context,
    egui_winit: egui_winit::State,
    egui_renderer: egui_wgpu::Renderer,
    chunk_renderer: ChunkRenderer,
}

fn make_msaa_view(
    device: &wgpu::Device,
    config: &wgpu::SurfaceConfiguration,
) -> wgpu::TextureView {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("msaa color"),
        size: wgpu::Extent3d {
            width: config.width.max(1),
            height: config.height.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: MSAA_SAMPLES,
        dimension: wgpu::TextureDimension::D2,
        format: config.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    texture.create_view(&wgpu::TextureViewDescriptor::default())
}

impl RenderState {
    pub async fn new(window: Arc<Window>) -> Result<Self> {
        let size = window.inner_size();

        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let surface = instance.create_surface(window.clone())?;

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::default(),
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await?;

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("viewer device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                memory_hints: wgpu::MemoryHints::default(),
                trace: wgpu::Trace::Off,
            })
            .await?;

        let config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .ok_or_else(|| anyhow::anyhow!("surface incompatible with adapter"))?;
        surface.configure(&device, &config);

        let info_ = adapter.get_info();
        info!(adapter = %info_.name, backend = ?info_.backend, "wgpu adapter selected");

        let egui_ctx = egui::Context::default();
        let egui_winit = egui_winit::State::new(
            egui_ctx.clone(),
            egui::ViewportId::ROOT,
            &*window,
            Some(window.scale_factor() as f32),
            None,
            Some(2048),
        );
        let egui_renderer =
            egui_wgpu::Renderer::new(&device, config.format, None, MSAA_SAMPLES, false);

        let chunk_renderer = ChunkRenderer::new(&device, config.format);
        let msaa_view = make_msaa_view(&device, &config);

        Ok(Self {
            window,
            surface,
            device,
            queue,
            config,
            msaa_view,
            egui_ctx,
            egui_winit,
            egui_renderer,
            chunk_renderer,
        })
    }

    pub fn window(&self) -> &Window {
        &self.window
    }

    pub fn width(&self) -> u32 {
        self.config.width
    }

    pub fn height(&self) -> u32 {
        self.config.height
    }

    pub fn handle_window_event(&mut self, event: &WindowEvent) -> egui_winit::EventResponse {
        self.egui_winit.on_window_event(&self.window, event)
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.config.width = width.max(1);
        self.config.height = height.max(1);
        self.surface.configure(&self.device, &self.config);
        self.msaa_view = make_msaa_view(&self.device, &self.config);
    }

    pub fn upload_chunks(&mut self, chunks: &[Chunk]) {
        self.chunk_renderer
            .upload_chunks(&self.device, &self.queue, chunks);
    }

    pub fn render(
        &mut self,
        network: &NetworkStatus,
        server_addr: SocketAddr,
        chunks: &[Chunk],
        camera: &Camera,
        layer_flags: &mut u32,
        sim_paused: &mut bool,
        sim_tick_hz: &mut u32,
        sim_tick: u64,
        cursor_px: Option<glam::Vec2>,
        context_menu: &mut Option<ContextMenu>,
        outgoing: &UnboundedSender<ClientMessage>,
    ) {
        let frame = match self.surface.get_current_texture() {
            Ok(f) => f,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                self.surface.configure(&self.device, &self.config);
                return;
            }
            Err(_) => return,
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let aspect = self.config.width as f32 / self.config.height.max(1) as f32;
        self.chunk_renderer
            .upload_camera(&self.queue, camera.view_proj(aspect));

        let cursor_world = cursor_px.map(|px| {
            camera.pixel_to_world(
                px,
                glam::vec2(self.config.width as f32, self.config.height as f32),
            )
        });
        let hovered_cell = cursor_world
            .map(|w| (w.x.floor() as i32, w.y.floor() as i32))
            .and_then(|(x, y)| find_cell(chunks, x, y));

        let raw_input = self.egui_winit.take_egui_input(&self.window);
        let egui_output = self.egui_ctx.run(raw_input, |ctx| {
            draw_ui(
                ctx,
                network,
                server_addr,
                chunks.len(),
                cursor_world,
                hovered_cell,
                layer_flags,
                sim_paused,
                sim_tick_hz,
                sim_tick,
                outgoing,
            );
            draw_context_menu(ctx, context_menu, chunks, outgoing);
        });

        self.chunk_renderer.upload_world(&self.queue, *layer_flags);
        self.egui_winit
            .handle_platform_output(&self.window, egui_output.platform_output);

        let paint_jobs = self
            .egui_ctx
            .tessellate(egui_output.shapes, egui_output.pixels_per_point);
        let screen_descriptor = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.config.width, self.config.height],
            pixels_per_point: egui_output.pixels_per_point,
        };

        for (id, image_delta) in &egui_output.textures_delta.set {
            self.egui_renderer
                .update_texture(&self.device, &self.queue, *id, image_delta);
        }

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame encoder"),
            });

        self.egui_renderer.update_buffers(
            &self.device,
            &self.queue,
            &mut encoder,
            &paint_jobs,
            &screen_descriptor,
        );

        {
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("main pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &self.msaa_view,
                        resolve_target: Some(&view),
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: 0.05,
                                g: 0.07,
                                b: 0.10,
                                a: 1.0,
                            }),
                            store: wgpu::StoreOp::Discard,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                })
                .forget_lifetime();
            self.chunk_renderer.draw(&mut pass);
            self.egui_renderer
                .render(&mut pass, &paint_jobs, &screen_descriptor);
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        frame.present();

        for id in &egui_output.textures_delta.free {
            self.egui_renderer.free_texture(id);
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct CameraUniform {
    view_proj: [[f32; 4]; 4],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct WorldUniform {
    layer_flags: u32,
    _pad: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ChunkInstance {
    chunk_pos: [f32; 2],
    chunk_first_cell: u32,
    _pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct GpuCell {
    organic: u32,
    soil_energy: u32,
    sunlit: u32,
    kind: u32,
    plant: u32,
    energy: u32,
    facing: u32,
    connections: u32,
}

const GPU_KIND_EMPTY: u32 = 0;
const GPU_KIND_LEAF: u32 = 1;
const GPU_KIND_ROOT: u32 = 2;
const GPU_KIND_STEM: u32 = 3;
const GPU_KIND_ANTENNA: u32 = 4;
const GPU_KIND_SPROUT: u32 = 5;
const GPU_KIND_SEED: u32 = 6;

struct ChunkRenderer {
    pipeline: wgpu::RenderPipeline,
    quad_buffer: wgpu::Buffer,
    instance_buffer: wgpu::Buffer,
    instance_capacity: u64,
    cells_buffer: wgpu::Buffer,
    cells_capacity: u64,
    camera_buffer: wgpu::Buffer,
    world_buffer: wgpu::Buffer,
    bind_group_layout: wgpu::BindGroupLayout,
    bind_group: wgpu::BindGroup,
    chunk_count: u32,
}

impl ChunkRenderer {
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("cell shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("cell.wgsl").into()),
        });

        let camera_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("camera"),
            size: std::mem::size_of::<CameraUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let world_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("world"),
            size: std::mem::size_of::<WorldUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let initial_cells_capacity = (1024 * std::mem::size_of::<GpuCell>()) as u64;
        let cells_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("cells"),
            size: initial_cells_capacity,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("cell bgl"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::VERTEX,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Storage { read_only: true },
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                ],
            });

        let bind_group = make_bind_group(
            device,
            &bind_group_layout,
            &camera_buffer,
            &world_buffer,
            &cells_buffer,
        );

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("cell pipeline layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("cell pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[
                    wgpu::VertexBufferLayout {
                        array_stride: 8,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &[wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x2,
                            offset: 0,
                            shader_location: 0,
                        }],
                    },
                    wgpu::VertexBufferLayout {
                        array_stride: std::mem::size_of::<ChunkInstance>() as u64,
                        step_mode: wgpu::VertexStepMode::Instance,
                        attributes: &[
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Float32x2,
                                offset: std::mem::offset_of!(ChunkInstance, chunk_pos) as u64,
                                shader_location: 1,
                            },
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Uint32,
                                offset: std::mem::offset_of!(ChunkInstance, chunk_first_cell)
                                    as u64,
                                shader_location: 2,
                            },
                        ],
                    },
                ],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState {
                count: MSAA_SAMPLES,
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            multiview: None,
            cache: None,
        });

        let quad: [[f32; 2]; 6] = [
            [0.0, 0.0],
            [1.0, 0.0],
            [0.0, 1.0],
            [0.0, 1.0],
            [1.0, 0.0],
            [1.0, 1.0],
        ];
        let quad_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("quad"),
            contents: bytemuck::cast_slice(&quad),
            usage: wgpu::BufferUsages::VERTEX,
        });

        let initial_instance_capacity = (16 * std::mem::size_of::<ChunkInstance>()) as u64;
        let instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("chunk instances"),
            size: initial_instance_capacity,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            pipeline,
            quad_buffer,
            instance_buffer,
            instance_capacity: initial_instance_capacity,
            cells_buffer,
            cells_capacity: initial_cells_capacity,
            camera_buffer,
            world_buffer,
            bind_group_layout,
            bind_group,
            chunk_count: 0,
        }
    }

    fn upload_camera(&self, queue: &wgpu::Queue, view_proj: glam::Mat4) {
        let uniform = CameraUniform {
            view_proj: view_proj.to_cols_array_2d(),
        };
        queue.write_buffer(&self.camera_buffer, 0, bytemuck::bytes_of(&uniform));
    }

    fn upload_world(&self, queue: &wgpu::Queue, layer_flags: u32) {
        let uniform = WorldUniform {
            layer_flags,
            _pad: [0; 3],
        };
        queue.write_buffer(&self.world_buffer, 0, bytemuck::bytes_of(&uniform));
    }

    fn upload_chunks(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, chunks: &[Chunk]) {
        if chunks.is_empty() {
            self.chunk_count = 0;
            return;
        }

        let instances: Vec<ChunkInstance> = chunks
            .iter()
            .enumerate()
            .map(|(i, chunk)| ChunkInstance {
                chunk_pos: [
                    chunk.coord.x as f32 * CHUNK_EDGE as f32,
                    chunk.coord.y as f32 * CHUNK_EDGE as f32,
                ],
                chunk_first_cell: (i * CHUNK_AREA) as u32,
                _pad: 0,
            })
            .collect();

        let chunk_lookup: std::collections::HashMap<(i32, i32), usize> = chunks
            .iter()
            .enumerate()
            .map(|(i, c)| ((c.coord.x, c.coord.y), i))
            .collect();

        let edge = CHUNK_EDGE as i32;
        let mut gpu_cells = Vec::with_capacity(chunks.len() * CHUNK_AREA);
        for chunk in chunks {
            let cx = chunk.coord.x;
            let cy = chunk.coord.y;
            for (i, cell) in chunk.cells.iter().enumerate() {
                let lx = (i % (CHUNK_EDGE as usize)) as i32;
                let ly = (i / (CHUNK_EDGE as usize)) as i32;
                let wx = cx * edge + lx;
                let wy = cy * edge + ly;
                let mut gc = to_gpu_cell(cell);
                if let Occupant::Stem { connections, .. } = &cell.occupant {
                    gc.connections = effective_stem_connections(
                        *connections,
                        chunks,
                        &chunk_lookup,
                        wx,
                        wy,
                    );
                }
                gpu_cells.push(gc);
            }
        }

        let inst_bytes: &[u8] = bytemuck::cast_slice(&instances);
        if inst_bytes.len() as u64 > self.instance_capacity {
            self.instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("chunk instances"),
                size: inst_bytes.len() as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.instance_capacity = inst_bytes.len() as u64;
        }
        queue.write_buffer(&self.instance_buffer, 0, inst_bytes);

        let cells_bytes: &[u8] = bytemuck::cast_slice(&gpu_cells);
        let needed = cells_bytes.len() as u64;
        if needed > self.cells_capacity {
            self.cells_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("cells"),
                size: needed,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.cells_capacity = needed;
            self.bind_group = make_bind_group(
                device,
                &self.bind_group_layout,
                &self.camera_buffer,
                &self.world_buffer,
                &self.cells_buffer,
            );
        }
        queue.write_buffer(&self.cells_buffer, 0, cells_bytes);

        self.chunk_count = chunks.len() as u32;
    }

    fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        if self.chunk_count == 0 {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_vertex_buffer(0, self.quad_buffer.slice(..));
        pass.set_vertex_buffer(1, self.instance_buffer.slice(..));
        pass.draw(0..6, 0..self.chunk_count);
    }
}

fn make_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    camera: &wgpu::Buffer,
    world: &wgpu::Buffer,
    cells: &wgpu::Buffer,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("cell bg"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: camera.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: world.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: cells.as_entire_binding(),
            },
        ],
    })
}

fn to_gpu_cell(cell: &Cell) -> GpuCell {
    let (kind, plant, energy, facing, connections) = match &cell.occupant {
        Occupant::Empty => (GPU_KIND_EMPTY, 0, 0, 0, 0),
        Occupant::Leaf {
            plant,
            energy,
            facing,
            ..
        } => (
            GPU_KIND_LEAF,
            *plant,
            u32::from(*energy),
            facing_to_u32(*facing),
            0,
        ),
        Occupant::Root { plant, energy, .. } => {
            (GPU_KIND_ROOT, *plant, u32::from(*energy), 0, 0)
        }
        Occupant::Stem {
            plant,
            energy,
            connections,
            ..
        } => (
            GPU_KIND_STEM,
            *plant,
            u32::from(*energy),
            0,
            u32::from(*connections),
        ),
        Occupant::Antenna { plant, energy, .. } => {
            (GPU_KIND_ANTENNA, *plant, u32::from(*energy), 0, 0)
        }
        Occupant::Sprout {
            plant,
            energy,
            facing,
            ..
        } => (
            GPU_KIND_SPROUT,
            *plant,
            u32::from(*energy),
            facing_to_u32(*facing),
            0,
        ),
        Occupant::Seed {
            plant,
            energy,
            facing,
            ..
        } => (
            GPU_KIND_SEED,
            *plant,
            u32::from(*energy),
            facing_to_u32(*facing),
            0,
        ),
    };
    GpuCell {
        organic: u32::from(cell.organic),
        soil_energy: u32::from(cell.soil_energy),
        sunlit: cell.sunlit as u32,
        kind,
        plant,
        energy,
        facing,
        connections,
    }
}

fn facing_to_u32(d: Direction) -> u32 {
    match d {
        Direction::North => 0,
        Direction::East => 1,
        Direction::South => 2,
        Direction::West => 3,
    }
}

fn effective_stem_connections(
    raw: u8,
    chunks: &[Chunk],
    lookup: &std::collections::HashMap<(i32, i32), usize>,
    wx: i32,
    wy: i32,
) -> u32 {
    let mut out = 0u32;
    if raw & STEM_CONNECT_NORTH != 0 && neighbor_present(chunks, lookup, wx, wy - 1) {
        out |= STEM_CONNECT_NORTH as u32;
    }
    if raw & STEM_CONNECT_EAST != 0 && neighbor_present(chunks, lookup, wx + 1, wy) {
        out |= STEM_CONNECT_EAST as u32;
    }
    if raw & STEM_CONNECT_SOUTH != 0 && neighbor_present(chunks, lookup, wx, wy + 1) {
        out |= STEM_CONNECT_SOUTH as u32;
    }
    if raw & STEM_CONNECT_WEST != 0 && neighbor_present(chunks, lookup, wx - 1, wy) {
        out |= STEM_CONNECT_WEST as u32;
    }
    out
}

fn neighbor_present(
    chunks: &[Chunk],
    lookup: &std::collections::HashMap<(i32, i32), usize>,
    wx: i32,
    wy: i32,
) -> bool {
    let edge = CHUNK_EDGE as i32;
    let cx = wx.div_euclid(edge);
    let cy = wy.div_euclid(edge);
    let lx = wx.rem_euclid(edge) as usize;
    let ly = wy.rem_euclid(edge) as usize;
    let cell_idx = ly * (CHUNK_EDGE as usize) + lx;
    if let Some(&chunk_idx) = lookup.get(&(cx, cy)) {
        if let Some(cell) = chunks[chunk_idx].cells.get(cell_idx) {
            return !matches!(cell.occupant, Occupant::Empty);
        }
    }
    false
}

fn draw_ui(
    ctx: &egui::Context,
    network: &NetworkStatus,
    server_addr: SocketAddr,
    chunk_count: usize,
    cursor_world: Option<glam::Vec2>,
    hovered_cell: Option<(ChunkCoord, &Cell)>,
    layer_flags: &mut u32,
    sim_paused: &mut bool,
    sim_tick_hz: &mut u32,
    sim_tick: u64,
    outgoing: &UnboundedSender<ClientMessage>,
) {
    egui::Window::new("Status")
        .anchor(egui::Align2::LEFT_TOP, egui::vec2(10.0, 10.0))
        .resizable(false)
        .collapsible(false)
        .show(ctx, |ui| {
            match network {
                NetworkStatus::Connecting(None) => {
                    ui.label(format!("Connecting to {server_addr}..."));
                }
                NetworkStatus::Connecting(Some(reason)) => {
                    ui.colored_label(egui::Color32::LIGHT_RED, "Reconnecting...");
                    ui.label(format!("Server: {server_addr}"));
                    ui.weak(format!("Last error: {reason}"));
                }
                NetworkStatus::Connected {
                    world_chunks_x,
                    world_chunks_y,
                    seed,
                    ..
                } => {
                    ui.colored_label(egui::Color32::LIGHT_GREEN, "Connected");
                    ui.label(format!("Server: {server_addr}"));
                    ui.label(format!(
                        "World: {world_chunks_x} × {world_chunks_y} chunks"
                    ));
                    ui.label(format!("Seed: {seed:#018x}"));
                }
            }
            ui.separator();
            ui.label(format!("Loaded chunks: {chunk_count}"));
            ui.label("Drag = pan, Scroll = zoom, Right-click for menu");
            ui.separator();
            ui.label("Sim:");
            ui.label(format!("Tick: {sim_tick}"));
            ui.horizontal(|ui| {
                let label = if *sim_paused { "Resume" } else { "Pause" };
                if ui.button(label).clicked() {
                    *sim_paused = !*sim_paused;
                    let _ = outgoing.send(ClientMessage::SetPaused(*sim_paused));
                }
                if ui
                    .add_enabled(*sim_paused, egui::Button::new("Step"))
                    .clicked()
                {
                    let _ = outgoing.send(ClientMessage::Step);
                }
            });
            let mut hz = *sim_tick_hz;
            if ui
                .add(egui::Slider::new(&mut hz, 1..=60).text("Hz"))
                .changed()
            {
                *sim_tick_hz = hz;
                let _ = outgoing.send(ClientMessage::SetTickHz(hz));
            }
            ui.separator();
            ui.label("Layers:");
            let mut organic = (*layer_flags & LAYER_ORGANIC) != 0;
            let mut energy = (*layer_flags & LAYER_ENERGY) != 0;
            let mut fg = (*layer_flags & LAYER_FG) != 0;
            if ui.checkbox(&mut organic, "Organic").changed() {
                *layer_flags = (*layer_flags & !LAYER_ORGANIC)
                    | (if organic { LAYER_ORGANIC } else { 0 });
            }
            if ui.checkbox(&mut energy, "Energy").changed() {
                *layer_flags = (*layer_flags & !LAYER_ENERGY)
                    | (if energy { LAYER_ENERGY } else { 0 });
            }
            if ui.checkbox(&mut fg, "Occupants").changed() {
                *layer_flags = (*layer_flags & !LAYER_FG) | (if fg { LAYER_FG } else { 0 });
            }
            ui.separator();
            match cursor_world {
                Some(w) => {
                    ui.label(format!("Cursor: ({:.0}, {:.0})", w.x, w.y));
                    if let Some((coord, cell)) = hovered_cell {
                        ui.label(format!("Chunk: ({}, {})", coord.x, coord.y));
                        cell_details_ui(ui, cell);
                    } else {
                        ui.weak("(outside world)");
                    }
                }
                None => {
                    ui.label("Cursor: —");
                }
            }
        });
}

fn draw_context_menu(
    ctx: &egui::Context,
    context_menu: &mut Option<ContextMenu>,
    chunks: &[Chunk],
    outgoing: &UnboundedSender<ClientMessage>,
) {
    let Some(menu) = *context_menu else {
        return;
    };
    let cell_at_menu = find_cell(chunks, menu.world_x, menu.world_y);

    egui::Area::new(egui::Id::new("context-menu"))
        .fixed_pos([menu.screen_pos.x, menu.screen_pos.y])
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_min_width(200.0);
                ui.label(
                    egui::RichText::new(format!("Cell ({}, {})", menu.world_x, menu.world_y))
                        .strong(),
                );
                ui.separator();
                if let Some((coord, cell)) = cell_at_menu {
                    ui.label(format!("Chunk: ({}, {})", coord.x, coord.y));
                    cell_details_ui(ui, cell);
                    ui.separator();
                    if ui.button("Spawn sprout (facing N)").clicked() {
                        let _ = outgoing.send(ClientMessage::SpawnSprout {
                            x: menu.world_x,
                            y: menu.world_y,
                            facing: Direction::North,
                        });
                        *context_menu = None;
                    }
                } else {
                    ui.weak("(outside world)");
                    ui.separator();
                }
                if ui.button("Close").clicked() {
                    *context_menu = None;
                }
            });
        });
}

fn cell_details_ui(ui: &mut egui::Ui, cell: &Cell) {
    ui.label(format!("organic: {}", cell.organic));
    ui.label(format!("soil_energy: {}", cell.soil_energy));
    ui.label(format!("sunlit: {}", cell.sunlit));
    ui.label(format!("occupant: {}", occupant_label(&cell.occupant)));
}

fn occupant_label(occ: &Occupant) -> String {
    match occ {
        Occupant::Empty => "empty".to_string(),
        Occupant::Leaf {
            plant,
            energy,
            facing,
            parent,
        } => format!(
            "leaf (plant {plant}, energy {energy}, facing {facing:?}, parent {})",
            parent_label(*parent)
        ),
        Occupant::Root {
            plant,
            energy,
            parent,
        } => format!(
            "root (plant {plant}, energy {energy}, parent {})",
            parent_label(*parent)
        ),
        Occupant::Stem {
            plant,
            energy,
            connections,
            parent,
            children,
        } => format!(
            "stem (plant {plant}, energy {energy}, conn {}, parent {}, kids {})",
            connections_label(*connections),
            parent_label(*parent),
            connections_label(*children)
        ),
        Occupant::Antenna {
            plant,
            energy,
            parent,
        } => format!(
            "antenna (plant {plant}, energy {energy}, parent {})",
            parent_label(*parent)
        ),
        Occupant::Sprout {
            plant,
            energy,
            facing,
            parent,
            current_gene,
            ..
        } => format!(
            "sprout (plant {plant}, energy {energy}, facing {facing:?}, parent {}, gene {})",
            parent_label(*parent),
            current_gene
        ),
        Occupant::Seed {
            plant,
            energy,
            facing,
            parent,
            ..
        } => format!(
            "seed (plant {plant}, energy {energy}, facing {facing:?}, parent {})",
            parent_label(*parent)
        ),
    }
}

fn parent_label(p: Option<protocol::Direction>) -> String {
    match p {
        None => "—".to_string(),
        Some(d) => format!("{d:?}"),
    }
}

fn connections_label(c: u8) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if c & STEM_CONNECT_NORTH != 0 {
        parts.push("N");
    }
    if c & STEM_CONNECT_EAST != 0 {
        parts.push("E");
    }
    if c & STEM_CONNECT_SOUTH != 0 {
        parts.push("S");
    }
    if c & STEM_CONNECT_WEST != 0 {
        parts.push("W");
    }
    if parts.is_empty() {
        "—".to_string()
    } else {
        parts.join("|")
    }
}

fn find_cell(chunks: &[Chunk], x: i32, y: i32) -> Option<(ChunkCoord, &Cell)> {
    let edge = CHUNK_EDGE as i32;
    let cx = x.div_euclid(edge);
    let cy = y.div_euclid(edge);
    let lx = x.rem_euclid(edge) as usize;
    let ly = y.rem_euclid(edge) as usize;
    let cell_idx = ly * (CHUNK_EDGE as usize) + lx;
    let chunk = chunks.iter().find(|c| c.coord.x == cx && c.coord.y == cy)?;
    chunk.cells.get(cell_idx).map(|cell| (chunk.coord, cell))
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{CHUNK_AREA, Cell, ChunkCoord, Direction, Genome, Occupant};

    fn empty_chunk(cx: i32, cy: i32) -> Chunk {
        let cells = (0..CHUNK_AREA)
            .map(|_| Cell {
                organic: 0,
                soil_energy: 0,
                sunlit: false,
                occupant: Occupant::Empty,
            })
            .collect();
        Chunk {
            coord: ChunkCoord { x: cx, y: cy },
            cells,
        }
    }

    fn lookup_for(chunks: &[Chunk]) -> std::collections::HashMap<(i32, i32), usize> {
        chunks
            .iter()
            .enumerate()
            .map(|(i, c)| ((c.coord.x, c.coord.y), i))
            .collect()
    }

    #[test]
    fn facing_to_u32_enumerates_directions() {
        assert_eq!(facing_to_u32(Direction::North), 0);
        assert_eq!(facing_to_u32(Direction::East), 1);
        assert_eq!(facing_to_u32(Direction::South), 2);
        assert_eq!(facing_to_u32(Direction::West), 3);
    }

    #[test]
    fn parent_label_handles_none_and_each_direction() {
        assert_eq!(parent_label(None), "—");
        assert_eq!(parent_label(Some(Direction::North)), "North");
        assert_eq!(parent_label(Some(Direction::West)), "West");
    }

    #[test]
    fn connections_label_lists_only_set_bits() {
        assert_eq!(connections_label(0), "—");
        assert_eq!(connections_label(STEM_CONNECT_NORTH), "N");
        assert_eq!(
            connections_label(STEM_CONNECT_NORTH | STEM_CONNECT_SOUTH),
            "N|S"
        );
        assert_eq!(
            connections_label(
                STEM_CONNECT_NORTH | STEM_CONNECT_EAST | STEM_CONNECT_SOUTH | STEM_CONNECT_WEST
            ),
            "N|E|S|W"
        );
    }

    #[test]
    fn occupant_label_includes_kind_for_each_variant() {
        assert_eq!(occupant_label(&Occupant::Empty), "empty");

        let leaf = Occupant::Leaf {
            plant: 1,
            energy: 50,
            facing: Direction::North,
            parent: Some(Direction::South),
        };
        assert!(occupant_label(&leaf).starts_with("leaf"));

        let stem = Occupant::Stem {
            plant: 1,
            energy: 0,
            connections: STEM_CONNECT_NORTH,
            parent: None,
            children: STEM_CONNECT_SOUTH,
        };
        let s = occupant_label(&stem);
        assert!(s.starts_with("stem"));
        assert!(s.contains("conn N"));
        assert!(s.contains("kids S"));

        let sprout = Occupant::Sprout {
            plant: 1,
            energy: 0,
            facing: Direction::North,
            genome: Box::new(Genome::default_vine()),
            parent: None,
            current_gene: 7,
        };
        assert!(occupant_label(&sprout).contains("gene 7"));
    }

    #[test]
    fn find_cell_returns_cell_at_world_coord() {
        let mut chunks = vec![empty_chunk(0, 0), empty_chunk(1, 0)];
        // Mark a cell in chunk (1, 0) so we can identify it.
        chunks[1].cells[0].organic = 99;

        // World (CHUNK_EDGE, 0) → chunk (1, 0), local (0, 0).
        let edge = CHUNK_EDGE as i32;
        let (coord, cell) = find_cell(&chunks, edge, 0).expect("hit");
        assert_eq!(coord, ChunkCoord { x: 1, y: 0 });
        assert_eq!(cell.organic, 99);

        // OOB returns None.
        assert!(find_cell(&chunks, -1, 0).is_none());
        assert!(find_cell(&chunks, edge * 5, 0).is_none());
    }

    #[test]
    fn neighbor_present_distinguishes_empty_from_occupied() {
        let mut chunks = vec![empty_chunk(0, 0)];
        chunks[0].cells[1].occupant = Occupant::Leaf {
            plant: 1,
            energy: 0,
            facing: Direction::North,
            parent: None,
        };
        let lookup = lookup_for(&chunks);

        // (1, 0) holds the leaf.
        assert!(neighbor_present(&chunks, &lookup, 1, 0));
        // (0, 0) is empty.
        assert!(!neighbor_present(&chunks, &lookup, 0, 0));
        // Outside any known chunk → false.
        assert!(!neighbor_present(&chunks, &lookup, -1, -1));
    }

    #[test]
    fn effective_stem_connections_masks_out_empty_neighbors() {
        let mut chunks = vec![empty_chunk(0, 0)];
        // Place a leaf to the North of (5, 5) — i.e. at (5, 4).
        chunks[0].cells[4 * (CHUNK_EDGE as usize) + 5].occupant = Occupant::Leaf {
            plant: 1,
            energy: 0,
            facing: Direction::North,
            parent: None,
        };
        let lookup = lookup_for(&chunks);

        let raw = STEM_CONNECT_NORTH | STEM_CONNECT_EAST;
        let eff = effective_stem_connections(raw, &chunks, &lookup, 5, 5);
        // North neighbor exists → bit kept. East neighbor empty → bit dropped.
        assert_eq!(eff, STEM_CONNECT_NORTH as u32);
    }
}
