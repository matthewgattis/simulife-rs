use std::{net::SocketAddr, sync::Arc};

use anyhow::Result;
use protocol::{
    CHUNK_EDGE, Cell, Chunk, ChunkCoord, ClientMessage, Direction, Occupant, STEM_CONNECT_EAST,
    STEM_CONNECT_NORTH, STEM_CONNECT_SOUTH, STEM_CONNECT_WEST,
};
use tokio::sync::mpsc::UnboundedSender;
use tracing::info;
use wgpu::util::DeviceExt;
use winit::{event::WindowEvent, window::Window};

use crate::app::{Camera, ContextMenu, NetworkStatus};

const MSAA_SAMPLES: u32 = 4;

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
    cell_renderer: CellRenderer,
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

        let cell_renderer = CellRenderer::new(&device, config.format);
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
            cell_renderer,
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

    pub fn render(
        &mut self,
        network: &NetworkStatus,
        server_addr: SocketAddr,
        chunks: &[Chunk],
        camera: &Camera,
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
        self.cell_renderer
            .upload_camera(&self.queue, camera.view_proj(aspect));

        let instances = build_instances(chunks);
        self.cell_renderer
            .upload_instances(&self.device, &self.queue, &instances);

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
            );
            draw_context_menu(ctx, context_menu, chunks, outgoing);
        });
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
            self.cell_renderer.draw(&mut pass, instances.len() as u32);
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
struct CellInstance {
    cell_pos: [f32; 2],
    bg_color: [f32; 3],
    fg_color: [f32; 3],
    shape: u32,
}

const SHAPE_NONE: u32 = 0;
const SHAPE_CIRCLE: u32 = 1;
const SHAPE_SQUARE: u32 = 2;
const SHAPE_OVAL_H: u32 = 3;
const SHAPE_OVAL_V: u32 = 4;
const SHAPE_STEM: u32 = 5;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct CameraUniform {
    view_proj: [[f32; 4]; 4],
}

struct CellRenderer {
    pipeline: wgpu::RenderPipeline,
    quad_buffer: wgpu::Buffer,
    instance_buffer: wgpu::Buffer,
    instance_capacity: u64,
    camera_buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
}

impl CellRenderer {
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

        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("camera bgl"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("camera bg"),
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: camera_buffer.as_entire_binding(),
            }],
        });

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
                        array_stride: std::mem::size_of::<CellInstance>() as u64,
                        step_mode: wgpu::VertexStepMode::Instance,
                        attributes: &[
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Float32x2,
                                offset: std::mem::offset_of!(CellInstance, cell_pos) as u64,
                                shader_location: 1,
                            },
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Float32x3,
                                offset: std::mem::offset_of!(CellInstance, bg_color) as u64,
                                shader_location: 2,
                            },
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Float32x3,
                                offset: std::mem::offset_of!(CellInstance, fg_color) as u64,
                                shader_location: 3,
                            },
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Uint32,
                                offset: std::mem::offset_of!(CellInstance, shape) as u64,
                                shader_location: 4,
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

        let initial_instance_capacity = (1024 * std::mem::size_of::<CellInstance>()) as u64;
        let instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("instances"),
            size: initial_instance_capacity,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            pipeline,
            quad_buffer,
            instance_buffer,
            instance_capacity: initial_instance_capacity,
            camera_buffer,
            bind_group,
        }
    }

    fn upload_camera(&self, queue: &wgpu::Queue, view_proj: glam::Mat4) {
        let uniform = CameraUniform {
            view_proj: view_proj.to_cols_array_2d(),
        };
        queue.write_buffer(&self.camera_buffer, 0, bytemuck::bytes_of(&uniform));
    }

    fn upload_instances(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        instances: &[CellInstance],
    ) {
        if instances.is_empty() {
            return;
        }
        let bytes: &[u8] = bytemuck::cast_slice(instances);
        if bytes.len() as u64 > self.instance_capacity {
            self.instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("instances"),
                size: bytes.len() as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.instance_capacity = bytes.len() as u64;
        }
        queue.write_buffer(&self.instance_buffer, 0, bytes);
    }

    fn draw(&self, pass: &mut wgpu::RenderPass<'_>, instance_count: u32) {
        if instance_count == 0 {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_vertex_buffer(0, self.quad_buffer.slice(..));
        pass.set_vertex_buffer(1, self.instance_buffer.slice(..));
        pass.draw(0..6, 0..instance_count);
    }
}

fn build_instances(chunks: &[Chunk]) -> Vec<CellInstance> {
    let edge = CHUNK_EDGE as usize;
    let mut instances = Vec::with_capacity(chunks.len() * edge * edge);
    for chunk in chunks {
        let cx = chunk.coord.x as f32 * edge as f32;
        let cy = chunk.coord.y as f32 * edge as f32;
        for (i, cell) in chunk.cells.iter().enumerate() {
            let lx = (i % edge) as f32;
            let ly = (i / edge) as f32;
            let bg = soil_tint(cell.sunlit, cell.organic);
            let (fg, shape) = occupant_visual(&cell.occupant);
            instances.push(CellInstance {
                cell_pos: [cx + lx, cy + ly],
                bg_color: bg,
                fg_color: fg,
                shape,
            });
        }
    }
    instances
}

fn soil_tint(sunlit: bool, organic: u16) -> [f32; 3] {
    let base = if sunlit { 0.18 } else { 0.10 };
    let organic_tint = (organic as f32) / 255.0 * 0.18;
    [
        base + organic_tint,
        base + organic_tint * 0.6,
        base * 0.8,
    ]
}

fn occupant_visual(occ: &Occupant) -> ([f32; 3], u32) {
    match occ {
        Occupant::Empty => ([0.0; 3], SHAPE_NONE),
        Occupant::Leaf { facing, .. } => {
            let kind = match facing {
                Direction::North | Direction::South => SHAPE_OVAL_V,
                Direction::East | Direction::West => SHAPE_OVAL_H,
            };
            ([0.20, 0.75, 0.30], kind)
        }
        Occupant::Root { .. } => ([0.50, 0.30, 0.10], SHAPE_SQUARE),
        Occupant::Stem { connections, .. } => (
            [0.55, 0.45, 0.25],
            SHAPE_STEM | ((*connections as u32) << 8),
        ),
        Occupant::Antenna { .. } => ([0.30, 0.55, 0.95], SHAPE_CIRCLE),
        Occupant::Sprout { .. } => ([1.00, 0.85, 0.20], SHAPE_CIRCLE),
        Occupant::Seed { .. } => ([0.80, 0.70, 0.35], SHAPE_CIRCLE),
    }
}

fn draw_ui(
    ctx: &egui::Context,
    network: &NetworkStatus,
    server_addr: SocketAddr,
    chunk_count: usize,
    cursor_world: Option<glam::Vec2>,
    hovered_cell: Option<(ChunkCoord, &Cell)>,
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
                } => {
                    ui.colored_label(egui::Color32::LIGHT_GREEN, "Connected");
                    ui.label(format!("Server: {server_addr}"));
                    ui.label(format!(
                        "World: {world_chunks_x} × {world_chunks_y} chunks"
                    ));
                }
            }
            ui.separator();
            ui.label(format!("Loaded chunks: {chunk_count}"));
            ui.label("Drag = pan, Scroll = zoom, Right-click for menu");
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
        } => format!("leaf (plant {plant}, energy {energy}, facing {facing:?})"),
        Occupant::Root { plant, energy } => {
            format!("root (plant {plant}, energy {energy})")
        }
        Occupant::Stem {
            plant,
            energy,
            connections,
        } => format!(
            "stem (plant {plant}, energy {energy}, conn {})",
            connections_label(*connections)
        ),
        Occupant::Antenna { plant, energy } => {
            format!("antenna (plant {plant}, energy {energy})")
        }
        Occupant::Sprout {
            plant,
            energy,
            facing,
            ..
        } => format!("sprout (plant {plant}, energy {energy}, facing {facing:?})"),
        Occupant::Seed {
            plant,
            energy,
            facing,
            ..
        } => format!("seed (plant {plant}, energy {energy}, facing {facing:?})"),
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
