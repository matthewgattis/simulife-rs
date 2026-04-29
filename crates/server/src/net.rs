use std::sync::{Arc, atomic::Ordering};

use anyhow::Result;
use protocol::{ClientMessage, ServerMessage};
use quinn::Endpoint;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use crate::sim::{self, SimState};

/// Push a fresh `Welcome` containing the authoritative paused / tick_hz /
/// tick / seed to every connected viewer. Used when a client-issued
/// control change (pause, tick rate) modifies global sim state — every
/// viewer's UI mirror needs to update, not just the requester's.
fn broadcast_sim_status(state: &SimState) {
    let (paused, tick_hz) = {
        let ctrl = state.control.lock().expect("control poisoned");
        (ctrl.paused, ctrl.tick_hz)
    };
    let welcome = ServerMessage::Welcome {
        world_chunks_x: state.chunks_x,
        world_chunks_y: state.chunks_y,
        paused,
        tick_hz,
        tick: state.current_tick.load(Ordering::Relaxed),
        seed: state.seed.load(Ordering::Relaxed),
    };
    if let Ok(bytes) = protocol::encode_server_message(&welcome) {
        let _ = state.tick_tx.send(Arc::new(bytes));
    }
}

pub async fn serve(state: Arc<SimState>, endpoint: Endpoint) {
    while let Some(incoming) = endpoint.accept().await {
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(e) = handle_connection(incoming, state).await {
                error!("connection error: {e:#}");
            }
        });
    }
}

async fn handle_connection(incoming: quinn::Incoming, state: Arc<SimState>) -> Result<()> {
    let conn = incoming.await?;
    let remote = conn.remote_address();
    info!(%remote, "connection accepted");

    let push_conn = conn.clone();
    let push_rx = state.tick_tx.subscribe();
    let push_task = tokio::spawn(async move {
        if let Err(e) = push_loop(push_conn, push_rx).await {
            warn!("push loop ended: {e:#}");
        }
    });

    let uni_conn = conn.clone();
    let uni_state = Arc::clone(&state);
    let uni_task = tokio::spawn(async move {
        accept_client_uni_streams(uni_conn, uni_state).await;
    });

    let result = handle_request_streams(conn, state).await;
    push_task.abort();
    uni_task.abort();
    result
}

async fn accept_client_uni_streams(conn: quinn::Connection, state: Arc<SimState>) {
    loop {
        let recv = match conn.accept_uni().await {
            Ok(r) => r,
            Err(_) => return,
        };
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(e) = handle_client_uni(recv, state).await {
                warn!("client uni stream error: {e:#}");
            }
        });
    }
}

async fn handle_client_uni(mut recv: quinn::RecvStream, state: Arc<SimState>) -> Result<()> {
    let buf = recv.read_to_end(64 * 1024).await?;
    let msg: ClientMessage = rmp_serde::from_slice(&buf)?;
    debug!(?msg, "received client command");

    match msg {
        ClientMessage::SpawnSprout { x, y, facing } => {
            sim::spawn_sprout(&state, x, y, facing);
        }
        ClientMessage::SetPaused(paused) => {
            {
                let mut ctrl = state.control.lock().expect("control poisoned");
                ctrl.paused = paused;
            }
            info!(paused, "sim pause state changed");
            broadcast_sim_status(&state);
        }
        ClientMessage::Step => {
            let mut ctrl = state.control.lock().expect("control poisoned");
            ctrl.step_pending = ctrl.step_pending.saturating_add(1);
            debug!(step_pending = ctrl.step_pending, "step requested");
        }
        ClientMessage::SetTickHz(hz) => {
            let hz = hz.max(1);
            {
                let mut ctrl = state.control.lock().expect("control poisoned");
                ctrl.tick_hz = hz;
            }
            info!(tick_hz = hz, "tick rate changed");
            broadcast_sim_status(&state);
        }
        ClientMessage::RegenerateWorld { seed } => {
            sim::regenerate_world(&state, seed);
        }
        other => warn!(?other, "unexpected message on client uni stream"),
    }
    Ok(())
}

async fn handle_request_streams(conn: quinn::Connection, state: Arc<SimState>) -> Result<()> {
    let remote = conn.remote_address();
    loop {
        let stream = match conn.accept_bi().await {
            Ok(s) => s,
            Err(_) => {
                info!(%remote, "connection closed");
                return Ok(());
            }
        };
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(e) = handle_stream(stream, state).await {
                error!("stream error: {e:#}");
            }
        });
    }
}

async fn handle_stream(
    (mut send, mut recv): (quinn::SendStream, quinn::RecvStream),
    state: Arc<SimState>,
) -> Result<()> {
    let buf = recv.read_to_end(64 * 1024).await?;
    let msg: ClientMessage = rmp_serde::from_slice(&buf)?;
    debug!(?msg, "received");

    let response = match msg {
        ClientMessage::Hello => {
            let (paused, tick_hz) = {
                let ctrl = state.control.lock().expect("control poisoned");
                (ctrl.paused, ctrl.tick_hz)
            };
            Some(ServerMessage::Welcome {
                world_chunks_x: state.chunks_x,
                world_chunks_y: state.chunks_y,
                paused,
                tick_hz,
                tick: state.current_tick.load(Ordering::Relaxed),
                seed: state.seed.load(Ordering::Relaxed),
            })
        }
        ClientMessage::Subscribe => {
            let chunks: Vec<protocol::WireChunk> = state
                .world
                .lock()
                .expect("sim lock poisoned")
                .iter()
                .map(protocol::WireChunk::from)
                .collect();
            let tick = state.current_tick.load(Ordering::Relaxed);
            Some(ServerMessage::ChunkBatch { tick, chunks })
        }
        ClientMessage::SpawnSprout { .. }
        | ClientMessage::SetPaused(_)
        | ClientMessage::Step
        | ClientMessage::SetTickHz(_)
        | ClientMessage::RegenerateWorld { .. } => {
            warn!("control / spawn message arrived on bidi stream; expected on uni");
            None
        }
    };
    if let Some(response) = response {
        let bytes = protocol::encode_server_message(&response)?;
        send.write_all(&bytes).await?;
    }
    send.finish()?;

    Ok(())
}

async fn push_loop(
    conn: quinn::Connection,
    mut rx: broadcast::Receiver<Arc<Vec<u8>>>,
) -> Result<()> {
    use tokio::sync::broadcast::error::RecvError;
    loop {
        let bytes = match rx.recv().await {
            Ok(b) => b,
            Err(RecvError::Lagged(n)) => {
                warn!(skipped = n, "push receiver lagged");
                continue;
            }
            Err(RecvError::Closed) => return Ok(()),
        };
        let mut send = match conn.open_uni().await {
            Ok(s) => s,
            Err(_) => return Ok(()),
        };
        send.write_all(&bytes).await?;
        send.finish()?;
    }
}
