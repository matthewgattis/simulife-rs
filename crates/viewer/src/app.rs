use std::{net::SocketAddr, sync::Arc};

use protocol::{CHUNK_EDGE, Chunk, ClientMessage};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::{debug, info};
use winit::{
    application::ApplicationHandler,
    event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, EventLoopProxy},
    keyboard::{Key, NamedKey},
    window::{Window, WindowId},
};

use crate::net;
use crate::render::RenderState;

#[derive(Debug, Clone)]
pub enum UserEvent {
    Network(NetworkStatus),
    Chunks(Vec<Chunk>),
}

#[derive(Debug, Clone)]
pub enum NetworkStatus {
    Connecting,
    Connected { world_chunks_x: u32, world_chunks_y: u32 },
    Failed(String),
}

#[derive(Debug, Clone, Copy)]
pub struct Camera {
    pub center: glam::Vec2,
    pub cells_visible_y: f32,
}

#[derive(Debug, Clone, Copy)]
pub struct ContextMenu {
    pub world_x: i32,
    pub world_y: i32,
    pub screen_pos: glam::Vec2,
}

impl Camera {
    pub fn view_proj(&self, aspect: f32) -> glam::Mat4 {
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

    pub fn pixel_to_world(&self, pixel: glam::Vec2, window_size: glam::Vec2) -> glam::Vec2 {
        let cells_per_pixel = self.cells_visible_y / window_size.y.max(1.0);
        let offset = pixel - window_size * 0.5;
        self.center + offset * cells_per_pixel
    }
}

pub struct App {
    state: Option<RenderState>,
    network: NetworkStatus,
    chunks: Vec<Chunk>,
    camera: Camera,
    dragging: bool,
    last_cursor: Option<glam::Vec2>,
    context_menu: Option<ContextMenu>,
    server_addr: SocketAddr,
    outgoing: UnboundedSender<ClientMessage>,
    pending_outgoing_rx: Option<UnboundedReceiver<ClientMessage>>,
    proxy: EventLoopProxy<UserEvent>,
    rt: Arc<tokio::runtime::Runtime>,
    network_started: bool,
}

impl App {
    pub fn new(
        rt: Arc<tokio::runtime::Runtime>,
        proxy: EventLoopProxy<UserEvent>,
        server_addr: SocketAddr,
        outgoing_tx: UnboundedSender<ClientMessage>,
        outgoing_rx: UnboundedReceiver<ClientMessage>,
    ) -> Self {
        Self {
            state: None,
            network: NetworkStatus::Connecting,
            chunks: Vec::new(),
            camera: Camera {
                center: glam::Vec2::ZERO,
                cells_visible_y: 64.0,
            },
            dragging: false,
            last_cursor: None,
            context_menu: None,
            server_addr,
            outgoing: outgoing_tx,
            pending_outgoing_rx: Some(outgoing_rx),
            proxy,
            rt,
            network_started: false,
        }
    }
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
            let server_addr = self.server_addr;
            let outgoing_rx = self
                .pending_outgoing_rx
                .take()
                .expect("outgoing receiver consumed twice");
            self.rt.spawn(async move {
                if let Err(e) = net::run_client(server_addr, proxy.clone(), outgoing_rx).await {
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
                info!(?status, "network status");
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
                if chunks.len() != self.chunks.len() {
                    info!(count = chunks.len(), "world snapshot loaded");
                } else {
                    debug!(count = chunks.len(), "world ticked");
                }
                self.chunks = chunks;
            }
        }
        if let Some(state) = &self.state {
            state.window().request_redraw();
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

        let response = state.handle_window_event(&event);
        if response.repaint {
            state.window().request_redraw();
        }
        if response.consumed {
            return;
        }

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                state.resize(size.width, size.height);
                state.window().request_redraw();
            }
            WindowEvent::CursorMoved { position, .. } => {
                let cursor = glam::vec2(position.x as f32, position.y as f32);
                if self.dragging {
                    if let Some(last) = self.last_cursor {
                        let delta = cursor - last;
                        let cells_per_pixel =
                            self.camera.cells_visible_y / state.height().max(1) as f32;
                        self.camera.center -= delta * cells_per_pixel;
                        state.window().request_redraw();
                    }
                }
                self.last_cursor = Some(cursor);
            }
            WindowEvent::MouseInput {
                state: button_state,
                button,
                ..
            } => match (button, button_state) {
                (MouseButton::Right, ElementState::Pressed) => {
                    if let Some(cursor) = self.last_cursor {
                        let win_size = glam::vec2(
                            state.width().max(1) as f32,
                            state.height().max(1) as f32,
                        );
                        let world = self.camera.pixel_to_world(cursor, win_size);
                        let scale = state.window().scale_factor() as f32;
                        self.context_menu = Some(ContextMenu {
                            world_x: world.x.floor() as i32,
                            world_y: world.y.floor() as i32,
                            screen_pos: cursor / scale.max(1.0),
                        });
                        state.window().request_redraw();
                    }
                }
                (MouseButton::Left, ElementState::Pressed) => {
                    if self.context_menu.is_some() {
                        self.context_menu = None;
                        state.window().request_redraw();
                    } else {
                        self.dragging = true;
                    }
                }
                (MouseButton::Left, ElementState::Released) => {
                    self.dragging = false;
                }
                _ => {}
            },
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        logical_key: Key::Named(NamedKey::Escape),
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } => {
                if self.context_menu.is_some() {
                    self.context_menu = None;
                    state.window().request_redraw();
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
                state.window().request_redraw();
            }
            WindowEvent::RedrawRequested => {
                state.render(
                    &self.network,
                    self.server_addr,
                    &self.chunks,
                    &self.camera,
                    self.last_cursor,
                    &mut self.context_menu,
                    &self.outgoing,
                );
            }
            _ => {}
        }
    }
}
