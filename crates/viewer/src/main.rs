use std::sync::Arc;

use anyhow::{Result, bail};
use protocol::{CHUNK_EDGE, Cell, Chunk, ClientMessage, Occupant, ServerMessage};
use wgpu::util::DeviceExt;
use winit::{
    application::ApplicationHandler,
    event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy},
    window::{Window, WindowId},
};

const SERVER_ADDR: &str = "127.0.0.1:4433";

#[derive(Debug, Clone)]
enum UserEvent {
    Network(NetworkStatus),
    Chunks(Vec<Chunk>),
}

#[derive(Debug, Clone)]
enum NetworkStatus {
    Connecting,
    Connected { world_chunks_x: u32, world_chunks_y: u32 },
    Failed(String),
}

#[derive(Debug, Clone, Copy)]
struct Camera {
    center: glam::Vec2,
    cells_visible_y: f32,
}

impl Camera {
    fn view_proj(&self, aspect: f32) -> glam::Mat4 {
        let cells_y = self.cells_visible_y.max(1.0);
        let cells_x = cells_y * aspect;
        let scale_x = 2.0 / cells_x;
        let scale_y = -2.0 / cells_y;
        glam::Mat4::from_cols_array_2d(&[
            [scale_x, 0.0, 0.0, 0.0],
            [0.0, scale_y, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [-self.center.x * scale_x, -self.center.y * scale_y, 0.0, 1.0],
        ])
    }
}

fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install default rustls crypto provider");

    let rt = Arc::new(tokio::runtime::Runtime::new()?);

    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();

    let mut app = App {
        state: None,
        network: NetworkStatus::Connecting,
        chunks: Vec::new(),
        camera: Camera {
            center: glam::Vec2::ZERO,
            cells_visible_y: 64.0,
        },
        dragging: false,
        last_cursor: None,
        proxy,
        rt,
        network_started: false,
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

struct App {
    state: Option<RenderState>,
    network: NetworkStatus,
    chunks: Vec<Chunk>,
    camera: Camera,
    dragging: bool,
    last_cursor: Option<glam::Vec2>,
    proxy: EventLoopProxy<UserEvent>,
    rt: Arc<tokio::runtime::Runtime>,
    network_started: bool,
}

struct RenderState {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    egui_ctx: egui::Context,
    egui_winit: egui_winit::State,
    egui_renderer: egui_wgpu::Renderer,
    cell_renderer: CellRenderer,
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        let window = Arc::new(
            event_loop
                .create_window(
                    Window::default_attributes()
                        .with_title("cellular-automata viewer")
                        .with_inner_size(winit::dpi::LogicalSize::new(1024.0, 768.0)),
                )
                .expect("create window"),
        );
        let state = pollster::block_on(RenderState::new(window))
            .expect("initialize wgpu");
        self.state = Some(state);

        if !self.network_started {
            self.network_started = true;
            let proxy = self.proxy.clone();
            self.rt.spawn(async move {
                if let Err(e) = run_client(proxy.clone()).await {
                    let _ = proxy.send_event(UserEvent::Network(
                        NetworkStatus::Failed(format!("{e:#}")),
                    ));
                }
            });
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Network(status) => {
                println!("network: {status:?}");
                if let NetworkStatus::Connected {
                    world_chunks_x,
                    world_chunks_y,
                } = &status
                {
                    let edge = CHUNK_EDGE as f32;
                    let world_w = (*world_chunks_x as f32) * edge;
                    let world_h = (*world_chunks_y as f32) * edge;
                    self.camera.center = glam::vec2(world_w * 0.5, world_h * 0.5);
                    self.camera.cells_visible_y = world_h * 1.1;
                }
                self.network = status;
            }
            UserEvent::Chunks(chunks) => {
                println!("loaded {} chunks", chunks.len());
                self.chunks = chunks;
            }
        }
        if let Some(state) = &self.state {
            state.window.request_redraw();
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: WindowId,
        event: WindowEvent,
    ) {
        let Some(state) = self.state.as_mut() else {
            return;
        };

        let response = state.egui_winit.on_window_event(&state.window, &event);
        if response.repaint {
            state.window.request_redraw();
        }
        if response.consumed {
            return;
        }

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                state.resize(size.width, size.height);
                state.window.request_redraw();
            }
            WindowEvent::CursorMoved { position, .. } => {
                let cursor = glam::vec2(position.x as f32, position.y as f32);
                if self.dragging {
                    if let Some(last) = self.last_cursor {
                        let delta = cursor - last;
                        let cells_per_pixel =
                            self.camera.cells_visible_y / state.config.height.max(1) as f32;
                        self.camera.center -= delta * cells_per_pixel;
                        state.window.request_redraw();
                    }
                }
                self.last_cursor = Some(cursor);
            }
            WindowEvent::MouseInput {
                state: button_state,
                button,
                ..
            } => {
                if button == MouseButton::Left {
                    self.dragging = button_state == ElementState::Pressed;
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let scroll = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(p) => (p.y as f32) / 50.0,
                };
                let factor = (-scroll * 0.1).exp();
                self.camera.cells_visible_y =
                    (self.camera.cells_visible_y * factor).clamp(4.0, 4096.0);
                state.window.request_redraw();
            }
            WindowEvent::RedrawRequested => {
                state.render(&self.network, &self.chunks, &self.camera);
            }
            _ => {}
        }
    }
}

impl RenderState {
    async fn new(window: Arc<Window>) -> Result<Self> {
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

        println!(
            "wgpu adapter: {} ({:?})",
            adapter.get_info().name,
            adapter.get_info().backend
        );

        let egui_ctx = egui::Context::default();
        let egui_winit = egui_winit::State::new(
            egui_ctx.clone(),
            egui::ViewportId::ROOT,
            &*window,
            Some(window.scale_factor() as f32),
            None,
            Some(2048),
        );
        let egui_renderer = egui_wgpu::Renderer::new(&device, config.format, None, 1, false);

        let cell_renderer = CellRenderer::new(&device, config.format);

        Ok(Self {
            window,
            surface,
            device,
            queue,
            config,
            egui_ctx,
            egui_winit,
            egui_renderer,
            cell_renderer,
        })
    }

    fn resize(&mut self, width: u32, height: u32) {
        self.config.width = width.max(1);
        self.config.height = height.max(1);
        self.surface.configure(&self.device, &self.config);
    }

    fn render(&mut self, network: &NetworkStatus, chunks: &[Chunk], camera: &Camera) {
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

        let raw_input = self.egui_winit.take_egui_input(&self.window);
        let egui_output = self.egui_ctx.run(raw_input, |ctx| {
            draw_ui(ctx, network, chunks.len());
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
                        view: &view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: 0.05,
                                g: 0.07,
                                b: 0.10,
                                a: 1.0,
                            }),
                            store: wgpu::StoreOp::Store,
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
    color: [f32; 3],
}

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
                                offset: 0,
                                shader_location: 1,
                            },
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Float32x3,
                                offset: 8,
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
            multisample: wgpu::MultisampleState::default(),
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

        let initial_instance_capacity =
            (1024 * std::mem::size_of::<CellInstance>()) as u64;
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
            instances.push(CellInstance {
                cell_pos: [cx + lx, cy + ly],
                color: cell_color(cell),
            });
        }
    }
    instances
}

fn cell_color(cell: &Cell) -> [f32; 3] {
    match &cell.occupant {
        Occupant::Empty => {
            let base = if cell.sunlit { 0.18 } else { 0.10 };
            let organic_tint = (cell.organic as f32) / 255.0 * 0.18;
            [base + organic_tint, base + organic_tint * 0.6, base * 0.8]
        }
        Occupant::Leaf { .. } => [0.20, 0.75, 0.30],
        Occupant::Root { .. } => [0.50, 0.30, 0.10],
        Occupant::Stem { .. } => [0.55, 0.45, 0.25],
        Occupant::Antenna { .. } => [0.55, 0.30, 0.85],
        Occupant::Sprout { .. } => [1.00, 0.85, 0.20],
        Occupant::Seed { .. } => [0.80, 0.70, 0.35],
    }
}

fn draw_ui(ctx: &egui::Context, network: &NetworkStatus, chunk_count: usize) {
    egui::Window::new("Status")
        .anchor(egui::Align2::LEFT_TOP, egui::vec2(10.0, 10.0))
        .resizable(false)
        .collapsible(false)
        .show(ctx, |ui| {
            match network {
                NetworkStatus::Connecting => {
                    ui.label("Connecting to server...");
                }
                NetworkStatus::Connected {
                    world_chunks_x,
                    world_chunks_y,
                } => {
                    ui.colored_label(egui::Color32::LIGHT_GREEN, "Connected");
                    ui.label(format!("Server: {SERVER_ADDR}"));
                    ui.label(format!(
                        "World: {world_chunks_x} × {world_chunks_y} chunks"
                    ));
                }
                NetworkStatus::Failed(reason) => {
                    ui.colored_label(egui::Color32::LIGHT_RED, "Connection failed");
                    ui.label(reason);
                }
            }
            ui.separator();
            ui.label(format!("Loaded chunks: {chunk_count}"));
            ui.label("Drag = pan, Scroll = zoom");
        });
}

async fn run_client(proxy: EventLoopProxy<UserEvent>) -> Result<()> {
    let _ = proxy.send_event(UserEvent::Network(NetworkStatus::Connecting));

    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(make_insecure_client_config()?);

    let server_addr = SERVER_ADDR.parse()?;
    let conn = endpoint.connect(server_addr, "localhost")?.await?;
    println!("connected to {}", conn.remote_address());

    let welcome = request(&conn, &ClientMessage::Hello).await?;
    let (world_chunks_x, world_chunks_y) = match welcome {
        ServerMessage::Welcome {
            world_chunks_x,
            world_chunks_y,
        } => (world_chunks_x, world_chunks_y),
        other => bail!("unexpected first message: {other:?}"),
    };
    let _ = proxy.send_event(UserEvent::Network(NetworkStatus::Connected {
        world_chunks_x,
        world_chunks_y,
    }));

    let batch = request(&conn, &ClientMessage::Subscribe).await?;
    match batch {
        ServerMessage::ChunkBatch(chunks) => {
            println!("received {} chunks", chunks.len());
            let _ = proxy.send_event(UserEvent::Chunks(chunks));
        }
        other => bail!("expected ChunkBatch, got {other:?}"),
    }

    Ok(())
}

async fn request(conn: &quinn::Connection, msg: &ClientMessage) -> Result<ServerMessage> {
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(&rmp_serde::to_vec(msg)?).await?;
    send.finish()?;
    let buf = recv.read_to_end(8 * 1024 * 1024).await?;
    Ok(rmp_serde::from_slice(&buf)?)
}

fn make_insecure_client_config() -> Result<quinn::ClientConfig> {
    let crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
        .with_no_client_auth();
    let quic = quinn::crypto::rustls::QuicClientConfig::try_from(crypto)?;
    Ok(quinn::ClientConfig::new(Arc::new(quic)))
}

#[derive(Debug)]
struct SkipServerVerification;

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
