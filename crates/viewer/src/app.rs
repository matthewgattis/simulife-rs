use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use protocol::{CHUNK_EDGE, ClientMessage, WireChunk};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::{debug, info};
use winit::{
    application::ApplicationHandler,
    event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy},
    keyboard::{Key, NamedKey},
    window::{Window, WindowId},
};

use crate::net;
use crate::render::{
    LAYER_CLAN, LAYER_ENERGY, LAYER_FG, LAYER_MUTATION_RATE, LAYER_ORGANIC, RenderState,
};

#[derive(Debug, Clone)]
pub enum UserEvent {
    Network(NetworkStatus),
    Chunks { tick: u64, chunks: Vec<WireChunk> },
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum NetworkStatus {
    Connecting(Option<String>),
    Connected {
        world_chunks_x: u32,
        world_chunks_y: u32,
        paused: bool,
        tick_hz: u32,
        tick_rate_limited: bool,
        tick: u64,
        seed: u64,
    },
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

#[derive(Debug, Clone)]
pub struct RegenDialog {
    /// The seed text the user is editing. Accepts decimal or `0x`-prefixed
    /// hex; parsing happens at submit time.
    pub seed_text: String,
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
    chunks: Vec<WireChunk>,
    camera: Camera,
    layer_flags: u32,
    sim_paused: bool,
    sim_tick_hz: u32,
    sim_tick_rate_limited: bool,
    sim_tick: u64,
    centered_once: bool,
    dragging: bool,
    last_cursor: Option<glam::Vec2>,
    context_menu: Option<ContextMenu>,
    regen_dialog: Option<RegenDialog>,
    server_addr: SocketAddr,
    outgoing: UnboundedSender<ClientMessage>,
    pending_outgoing_rx: Option<UnboundedReceiver<ClientMessage>>,
    proxy: EventLoopProxy<UserEvent>,
    rt: Arc<tokio::runtime::Runtime>,
    network_started: bool,
    /// When true, log a per-tick timing line in `UserEvent::Chunks`.
    tick_metrics: bool,
}

impl App {
    pub fn new(
        rt: Arc<tokio::runtime::Runtime>,
        proxy: EventLoopProxy<UserEvent>,
        server_addr: SocketAddr,
        outgoing_tx: UnboundedSender<ClientMessage>,
        outgoing_rx: UnboundedReceiver<ClientMessage>,
        tick_metrics: bool,
    ) -> Self {
        Self {
            state: None,
            network: NetworkStatus::Connecting(None),
            chunks: Vec::new(),
            camera: Camera {
                center: glam::Vec2::ZERO,
                cells_visible_y: 64.0,
            },
            layer_flags: LAYER_ORGANIC | LAYER_FG | LAYER_ENERGY,
            sim_paused: false,
            sim_tick_hz: 10,
            sim_tick_rate_limited: false,
            sim_tick: 0,
            centered_once: false,
            dragging: false,
            last_cursor: None,
            context_menu: None,
            regen_dialog: None,
            server_addr,
            outgoing: outgoing_tx,
            pending_outgoing_rx: Some(outgoing_rx),
            proxy,
            rt,
            network_started: false,
            tick_metrics,
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        // Reactive mode: idle blocks the event loop. Redraws happen only
        // when egui asks (via repaint_delay after each render) or when we
        // explicitly request_redraw on a real signal (tick, input).
        event_loop.set_control_flow(ControlFlow::Wait);

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
            let tick_metrics = self.tick_metrics;
            self.rt.spawn(async move {
                net::run_client(server_addr, proxy, outgoing_rx, tick_metrics).await;
            });
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Shutdown => {
                event_loop.exit();
                return;
            }
            UserEvent::Network(status) => {
                info!(?status, "network status");
                if let NetworkStatus::Connected {
                    world_chunks_x,
                    world_chunks_y,
                    paused,
                    tick_hz,
                    tick_rate_limited,
                    tick,
                    ..
                } = &status
                {
                    if !self.centered_once {
                        let edge = CHUNK_EDGE as f32;
                        let world_w = (*world_chunks_x as f32) * edge;
                        let world_h = (*world_chunks_y as f32) * edge;
                        self.camera.center = glam::vec2(world_w * 0.5, world_h * 0.5);
                        self.camera.cells_visible_y = world_h * 1.1;
                        self.centered_once = true;
                    }
                    self.sim_paused = *paused;
                    self.sim_tick_hz = *tick_hz;
                    self.sim_tick_rate_limited = *tick_rate_limited;
                    self.sim_tick = *tick;
                }
                self.network = status;
            }
            UserEvent::Chunks { tick, chunks } => {
                let _apply_span = tracing::info_span!("tick_apply", tick).entered();
                let dispatch_start = if self.tick_metrics {
                    Some(Instant::now())
                } else {
                    None
                };
                if chunks.len() != self.chunks.len() {
                    info!(count = chunks.len(), tick, "world snapshot loaded");
                } else {
                    debug!(count = chunks.len(), tick, "world ticked");
                }
                let assign_start = self.tick_metrics.then(Instant::now);
                self.chunks = chunks;
                let assign_us = assign_start
                    .map(|t| t.elapsed().as_micros() as u64)
                    .unwrap_or(0);
                self.sim_tick = tick;
                let upload_us = if let Some(state) = self.state.as_mut() {
                    let _upload_span = tracing::info_span!("upload_chunks").entered();
                    let t = self.tick_metrics.then(Instant::now);
                    state.upload_chunks(&self.chunks);
                    t.map(|t| t.elapsed().as_micros() as u64).unwrap_or(0)
                } else {
                    0
                };
                if self.tick_metrics {
                    let total_us = dispatch_start
                        .map(|t| t.elapsed().as_micros() as u64)
                        .unwrap_or(0);
                    info!(tick, assign_us, upload_us, total_us, "tick applied");
                }
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

        // Don't feed RedrawRequested to egui — it isn't an input event and
        // some integrations return repaint=true for it, which creates a
        // tight redraw loop in reactive mode.
        let response = if matches!(event, WindowEvent::RedrawRequested) {
            egui_winit::EventResponse {
                consumed: false,
                repaint: false,
            }
        } else {
            state.handle_window_event(&event)
        };
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
                        logical_key,
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } => match logical_key {
                Key::Named(NamedKey::Escape) => {
                    if self.context_menu.is_some() {
                        self.context_menu = None;
                        state.window().request_redraw();
                    }
                }
                Key::Named(NamedKey::Space) => {
                    // Server is authoritative — request the toggle; the
                    // broadcast Welcome will update self.sim_paused.
                    let _ = self
                        .outgoing
                        .send(ClientMessage::SetPaused(!self.sim_paused));
                }
                Key::Character(c) if c.as_str() == "." => {
                    // Step inherently pauses (server-side); works whether
                    // currently running or paused.
                    let _ = self.outgoing.send(ClientMessage::Step);
                }
                Key::Character(c) if matches!(c.as_str(), "1" | "2" | "3" | "4" | "5") => {
                    // Layer toggles, in panel order: 1=Organic, 2=Energy,
                    // 3=Occupants, 4=Clan colors, 5=Mutation rate.
                    let bit = match c.as_str() {
                        "1" => LAYER_ORGANIC,
                        "2" => LAYER_ENERGY,
                        "3" => LAYER_FG,
                        "4" => LAYER_CLAN,
                        "5" => LAYER_MUTATION_RATE,
                        _ => unreachable!(),
                    };
                    self.layer_flags ^= bit;
                    state.window().request_redraw();
                }
                _ => {}
            },
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
                let repaint_delay = state.render(
                    &self.network,
                    self.server_addr,
                    &self.chunks,
                    &self.camera,
                    &mut self.layer_flags,
                    &mut self.sim_paused,
                    &mut self.sim_tick_hz,
                    &mut self.sim_tick_rate_limited,
                    self.sim_tick,
                    self.last_cursor,
                    &mut self.context_menu,
                    &mut self.regen_dialog,
                    &self.outgoing,
                );
                // egui tells us when it next wants a frame (animation,
                // hover effects, etc). Schedule a wake-up if finite;
                // otherwise stay in Wait until a real event arrives.
                if repaint_delay == Duration::ZERO {
                    state.window().request_redraw();
                } else if repaint_delay < Duration::MAX {
                    event_loop.set_control_flow(ControlFlow::WaitUntil(
                        Instant::now() + repaint_delay,
                    ));
                } else {
                    event_loop.set_control_flow(ControlFlow::Wait);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Camera;

    fn approx_eq(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn pixel_to_world_at_window_center_returns_camera_center() {
        let cam = Camera {
            center: glam::vec2(50.0, 30.0),
            cells_visible_y: 64.0,
        };
        let win = glam::vec2(800.0, 600.0);
        let world = cam.pixel_to_world(win * 0.5, win);
        assert!(approx_eq(world.x, 50.0));
        assert!(approx_eq(world.y, 30.0));
    }

    #[test]
    fn pixel_to_world_translates_with_pixel_offset() {
        // 64 cells visible across 600 pixels of height → ~9.375 px/cell.
        // A pixel offset of (window_size / 2) along Y = +300 px puts us
        // +32 cells from center.
        let cam = Camera {
            center: glam::vec2(0.0, 0.0),
            cells_visible_y: 64.0,
        };
        let win = glam::vec2(800.0, 600.0);
        let world = cam.pixel_to_world(glam::vec2(400.0, 600.0), win);
        assert!(approx_eq(world.x, 0.0));
        assert!(approx_eq(world.y, 32.0));
    }

    #[test]
    fn pixel_to_world_scales_with_zoom() {
        // Zooming in (smaller cells_visible_y) → same pixel offset maps to
        // smaller world delta.
        let cam = Camera {
            center: glam::vec2(0.0, 0.0),
            cells_visible_y: 16.0, // 4× zoom vs 64
        };
        let win = glam::vec2(800.0, 600.0);
        let world = cam.pixel_to_world(glam::vec2(400.0, 600.0), win);
        // 16 cells across 600 px → +300 px = +8 cells.
        assert!(approx_eq(world.y, 8.0));
    }

    #[test]
    fn view_proj_is_invertible_around_camera_center() {
        // The matrix should map camera.center to NDC origin.
        let cam = Camera {
            center: glam::vec2(50.0, 30.0),
            cells_visible_y: 64.0,
        };
        let mat = cam.view_proj(800.0 / 600.0);
        let center_ndc = mat * glam::vec4(50.0, 30.0, 0.0, 1.0);
        assert!(approx_eq(center_ndc.x, 0.0));
        assert!(approx_eq(center_ndc.y, 0.0));
    }
}
