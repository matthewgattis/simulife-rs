use std::{
    collections::{HashMap, VecDeque},
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use protocol::{CHUNK_EDGE, ClientMessage, SimParams, WireChunk, WorldGenParams};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::{debug, info};
use winit::{
    application::ApplicationHandler,
    event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, Touch, TouchPhase, WindowEvent},
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
    /// Full snapshot — replace local chunks vec wholesale. Sent on
    /// initial Subscribe and after a regenerate.
    Chunks { tick: u64, chunks: Vec<WireChunk> },
    /// Per-tick delta — overlay each delta chunk onto the local vec
    /// by `coord`. Chunks not in the delta keep their previous state.
    ChunksDelta { tick: u64, chunks: Vec<WireChunk> },
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
        sim_params: SimParams,
        world_gen_params: WorldGenParams,
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

/// A single active touch point. Used to recognize one-finger pan,
/// two-finger pinch, and long-press from raw winit touch events.
#[derive(Debug, Clone, Copy)]
struct TouchPoint {
    start_pos: glam::Vec2,
    start_time: Instant,
    last_pos: glam::Vec2,
    /// Set at touch-down if the press landed on an egui panel/popup. Stays
    /// true for the touch's lifetime; suppresses canvas pan/pinch/long-press
    /// so the user can drag a slider or tap a button without affecting the
    /// world view.
    started_over_ui: bool,
    /// Set during 2-finger pinch. Suppresses long-press on lift but does
    /// NOT suppress further pinch updates the way `started_over_ui` would.
    pinched: bool,
}

const LONG_PRESS_DURATION: Duration = Duration::from_millis(500);
/// Maximum total movement (logical pixels) a touch may have undergone and
/// still count as a long-press rather than a drag.
const LONG_PRESS_MAX_MOVEMENT: f32 = 20.0;

#[derive(Debug, Clone)]
pub struct RegenDialog {
    /// The seed text the user is editing. Accepts decimal or `0x`-prefixed
    /// hex; parsing happens at submit time.
    pub seed_text: String,
    /// World-gen knobs to apply on Generate. Populated from the
    /// server's last `Welcome` when the dialog opens.
    pub params: WorldGenParams,
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

    /// Apply a zoom factor anchored at `anchor_pixel`: scale
    /// `cells_visible_y` by `factor` (clamped to [4, 4096]) and shift
    /// `center` so the world point under `anchor_pixel` stays put.
    pub fn zoom_around(
        &mut self,
        factor: f32,
        anchor_pixel: glam::Vec2,
        window_size: glam::Vec2,
    ) {
        let old_cells = self.cells_visible_y;
        let new_cells = (old_cells * factor).clamp(4.0, 4096.0);
        let offset = anchor_pixel - window_size * 0.5;
        self.center += offset * (old_cells - new_cells) / window_size.y.max(1.0);
        self.cells_visible_y = new_cells;
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
    /// Rolling 1-second window of (when-received, tick-number) pairs
    /// used to derive sim TPS. Each tick event pushes one entry;
    /// entries older than 1 s drop off the front.
    tps_samples: VecDeque<(Instant, u64)>,
    /// Most recently computed TPS — refreshed at most once per second
    /// so the UI readout doesn't flicker. NaN until enough data lands.
    sim_tps: f32,
    last_tps_update: Option<Instant>,
    sim_params: SimParams,
    world_gen_params: WorldGenParams,
    centered_once: bool,
    /// When false, draw_ui is skipped entirely so the world fills the
    /// whole window with no overlay (toggle with H). The context menu
    /// and regen dialog still render so the user can dismiss them.
    ui_visible: bool,
    dragging: bool,
    last_cursor: Option<glam::Vec2>,
    /// Active touch points keyed by winit Touch::id. Used for pan / pinch /
    /// long-press gesture recognition on Android.
    touches: HashMap<u64, TouchPoint>,
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
            tps_samples: VecDeque::new(),
            sim_tps: f32::NAN,
            last_tps_update: None,
            sim_params: SimParams::default(),
            world_gen_params: WorldGenParams::default(),
            centered_once: false,
            ui_visible: true,
            dragging: false,
            last_cursor: None,
            touches: HashMap::new(),
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

impl App {
    /// Record one tick reception, drop expired samples, and refresh
    /// the displayed TPS at most once per second. Computes TPS as
    /// `(latest_tick - oldest_tick_in_window) / (latest_time -
    /// oldest_time)` over the rolling 1-second window — robust to
    /// drops (uses tick numbers, not sample counts) and to brief
    /// pauses (window naturally shrinks).
    /// Recognize one-finger pan, two-finger pinch, and long-press from raw
    /// winit touch events. Touch IDs persist across Started/Moved/Ended for
    /// each finger; we keep per-id state in `self.touches`.
    fn handle_touch(&mut self, touch: &Touch) {
        let pos = glam::vec2(touch.location.x as f32, touch.location.y as f32);
        let Some(state) = self.state.as_mut() else {
            return;
        };

        match touch.phase {
            TouchPhase::Started => {
                let started_over_ui = state.point_over_ui(pos);
                self.touches.insert(
                    touch.id,
                    TouchPoint {
                        start_pos: pos,
                        start_time: Instant::now(),
                        last_pos: pos,
                        started_over_ui,
                        pinched: false,
                    },
                );
            }
            TouchPhase::Moved => match self.touches.len() {
                2 => {
                    // Pinch zoom: take old inter-touch distance, update this
                    // touch, take new distance, scale camera by the ratio.
                    // Skip if either finger landed on UI so widget drags
                    // (sliders, dialog moves) don't double as canvas zooms.
                    let ids: Vec<u64> = self.touches.keys().copied().collect();
                    let any_started_over_ui = self
                        .touches
                        .values()
                        .any(|t| t.started_over_ui);

                    let last_a = self.touches[&ids[0]].last_pos;
                    let last_b = self.touches[&ids[1]].last_pos;
                    let last_dist = (last_a - last_b).length();

                    if let Some(t) = self.touches.get_mut(&touch.id) {
                        t.last_pos = pos;
                    }
                    // Flag both as having pinched so long-press doesn't
                    // fire when the second finger lifts.
                    for t in self.touches.values_mut() {
                        t.pinched = true;
                    }

                    if any_started_over_ui {
                        return;
                    }

                    let new_a = self.touches[&ids[0]].last_pos;
                    let new_b = self.touches[&ids[1]].last_pos;
                    let new_dist = (new_a - new_b).length();

                    if last_dist > 1.0 && new_dist > 1.0 {
                        let factor = last_dist / new_dist;
                        let mid = (new_a + new_b) * 0.5;
                        let win_size = glam::vec2(
                            state.width().max(1) as f32,
                            state.height().max(1) as f32,
                        );
                        self.camera.zoom_around(factor, mid, win_size);
                        state.window().request_redraw();
                    }
                }
                1 => {
                    if let Some(t) = self.touches.get_mut(&touch.id) {
                        let delta = pos - t.last_pos;
                        t.last_pos = pos;
                        if t.started_over_ui {
                            return;
                        }
                        let cells_per_pixel =
                            self.camera.cells_visible_y / state.height().max(1) as f32;
                        self.camera.center -= delta * cells_per_pixel;
                        state.window().request_redraw();
                    }
                }
                _ => {}
            },
            TouchPhase::Ended | TouchPhase::Cancelled => {
                if let Some(t) = self.touches.remove(&touch.id) {
                    if t.started_over_ui || t.pinched {
                        return;
                    }
                    let held = t.start_time.elapsed();
                    let movement = (pos - t.start_pos).length();
                    let single_finger = self.touches.is_empty();
                    if single_finger
                        && held >= LONG_PRESS_DURATION
                        && movement <= LONG_PRESS_MAX_MOVEMENT
                    {
                        let win_size = glam::vec2(
                            state.width().max(1) as f32,
                            state.height().max(1) as f32,
                        );
                        let world = self.camera.pixel_to_world(pos, win_size);
                        let scale = state.window().scale_factor() as f32;
                        self.context_menu = Some(ContextMenu {
                            world_x: world.x.floor() as i32,
                            world_y: world.y.floor() as i32,
                            screen_pos: pos / scale.max(1.0),
                        });
                        state.window().request_redraw();
                    }
                }
            }
        }
    }

    fn record_tick(&mut self, tick: u64) {
        let now = Instant::now();
        self.tps_samples.push_back((now, tick));
        let window = Duration::from_secs(1);
        while let Some(&(t, _)) = self.tps_samples.front() {
            if now.duration_since(t) > window {
                self.tps_samples.pop_front();
            } else {
                break;
            }
        }
        let due = self
            .last_tps_update
            .map(|t| now.duration_since(t) >= window)
            .unwrap_or(true);
        if due {
            if let (Some(&(t0, n0)), Some(&(t1, n1))) =
                (self.tps_samples.front(), self.tps_samples.back())
            {
                let dt = t1.duration_since(t0).as_secs_f32();
                if dt > 0.0 && n1 > n0 {
                    self.sim_tps = (n1 - n0) as f32 / dt;
                    self.last_tps_update = Some(now);
                }
            }
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        // Drop GPU resources while the display connection is still alive.
        // On Linux (X11/Wayland), deferring this to normal struct drop
        // causes a segfault because winit tears down the display before
        // wgpu's Surface destructor runs.
        self.state = None;
    }

    fn suspended(&mut self, _event_loop: &ActiveEventLoop) {
        // Android destroys the GPU surface when the app backgrounds; drop
        // wgpu state so we don't render to a dead surface. resumed() will
        // recreate it.
        self.state = None;

        // Tear down the network task too: holding a QUIC connection (with
        // 2s keep-alives) while backgrounded burns battery and data, and
        // the server gets to free the slot. Replacing self.outgoing with a
        // fresh channel drops the old sender; the task's outgoing.recv()
        // returns None and it exits cleanly. resumed() will spawn a new one.
        let (new_tx, new_rx) = tokio::sync::mpsc::unbounded_channel();
        self.outgoing = new_tx;
        self.pending_outgoing_rx = Some(new_rx);
        self.network_started = false;
        self.network = NetworkStatus::Connecting(None);
    }

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
                    sim_params,
                    world_gen_params,
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
                    self.sim_params = *sim_params;
                    self.world_gen_params = *world_gen_params;
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
                self.record_tick(tick);
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
            UserEvent::ChunksDelta { tick, chunks: delta } => {
                let _apply_span = tracing::info_span!("tick_apply_delta", tick).entered();
                let dispatch_start = self.tick_metrics.then(Instant::now);
                let dirty = delta.len();
                // Overlay each delta chunk onto the local vec by
                // coord. Chunks_x derived from current network status
                // gives us O(1) index; we fall back to linear search
                // if dims are unknown (shouldn't happen post-Welcome).
                let world_chunks_x = match &self.network {
                    NetworkStatus::Connected { world_chunks_x, .. } => Some(*world_chunks_x),
                    _ => None,
                };
                let assign_start = self.tick_metrics.then(Instant::now);
                for incoming in delta {
                    let target_idx = match world_chunks_x {
                        Some(wx) => Some(
                            (incoming.coord.y as usize) * (wx as usize)
                                + (incoming.coord.x as usize),
                        ),
                        None => self
                            .chunks
                            .iter()
                            .position(|c| c.coord == incoming.coord),
                    };
                    if let Some(idx) = target_idx {
                        if let Some(slot) = self.chunks.get_mut(idx) {
                            *slot = incoming;
                        }
                    }
                }
                let assign_us = assign_start
                    .map(|t| t.elapsed().as_micros() as u64)
                    .unwrap_or(0);
                self.sim_tick = tick;
                self.record_tick(tick);
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
                    info!(tick, dirty, assign_us, upload_us, total_us, "delta applied");
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
                    // 3=Occupants. Keys 4 and 5 are radio-style — the
                    // tint they enable replaces the other tint
                    // (Clan/Mutation rate are mutually exclusive in UI).
                    match c.as_str() {
                        "1" => self.layer_flags ^= LAYER_ORGANIC,
                        "2" => self.layer_flags ^= LAYER_ENERGY,
                        "3" => self.layer_flags ^= LAYER_FG,
                        "4" => {
                            if (self.layer_flags & LAYER_CLAN) != 0 {
                                self.layer_flags &= !LAYER_CLAN;
                            } else {
                                self.layer_flags = (self.layer_flags & !LAYER_MUTATION_RATE)
                                    | LAYER_CLAN;
                            }
                        }
                        "5" => {
                            if (self.layer_flags & LAYER_MUTATION_RATE) != 0 {
                                self.layer_flags &= !LAYER_MUTATION_RATE;
                            } else {
                                self.layer_flags =
                                    (self.layer_flags & !LAYER_CLAN) | LAYER_MUTATION_RATE;
                            }
                        }
                        _ => unreachable!(),
                    }
                    state.window().request_redraw();
                }
                Key::Character(c) if matches!(c.as_str(), "h" | "H") => {
                    self.ui_visible = !self.ui_visible;
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
            WindowEvent::Touch(touch) => {
                self.handle_touch(&touch);
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
                    self.sim_tps,
                    &mut self.sim_params,
                    &self.world_gen_params,
                    self.last_cursor,
                    &mut self.context_menu,
                    &mut self.regen_dialog,
                    self.ui_visible,
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
    fn zoom_around_keeps_anchor_world_position_invariant() {
        let mut cam = Camera {
            center: glam::vec2(50.0, 30.0),
            cells_visible_y: 64.0,
        };
        let win = glam::vec2(800.0, 600.0);
        let anchor = glam::vec2(200.0, 450.0);
        let world_before = cam.pixel_to_world(anchor, win);
        cam.zoom_around(0.5, anchor, win);
        let world_after = cam.pixel_to_world(anchor, win);
        assert!(approx_eq(world_before.x, world_after.x));
        assert!(approx_eq(world_before.y, world_after.y));
    }

    #[test]
    fn zoom_around_window_center_leaves_camera_center_fixed() {
        let mut cam = Camera {
            center: glam::vec2(50.0, 30.0),
            cells_visible_y: 64.0,
        };
        let win = glam::vec2(800.0, 600.0);
        let center_before = cam.center;
        cam.zoom_around(0.5, win * 0.5, win);
        assert!(approx_eq(cam.center.x, center_before.x));
        assert!(approx_eq(cam.center.y, center_before.y));
        assert!(approx_eq(cam.cells_visible_y, 32.0));
    }

    #[test]
    fn zoom_around_clamps_cells_visible_y() {
        let mut cam = Camera {
            center: glam::vec2(0.0, 0.0),
            cells_visible_y: 8.0,
        };
        let win = glam::vec2(800.0, 600.0);
        cam.zoom_around(0.1, win * 0.5, win);
        assert!(approx_eq(cam.cells_visible_y, 4.0));
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
