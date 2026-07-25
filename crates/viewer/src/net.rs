use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use protocol::{ClientMessage, ServerMessage};
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{Instrument, debug, info, warn};
use winit::event_loop::EventLoopProxy;

use crate::app::{NetworkStatus, UserEvent};

const RECONNECT_DELAY: Duration = Duration::from_secs(1);
const KEEP_ALIVE: Duration = Duration::from_secs(2);
const IDLE_TIMEOUT: Duration = Duration::from_secs(6);

/// Assumed when the user types a bare host with no `:port`.
const DEFAULT_PORT: u16 = 4433;

/// Appends [`DEFAULT_PORT`] when `addr` has no port, so the connect dialog
/// accepts a bare `example.com` or `192.168.0.10`.
///
/// An unbracketed IPv6 literal (`::1`) is indistinguishable from a
/// host:port pair here and is left alone — bracket it (`[::1]`) to get the
/// default port appended.
pub fn with_default_port(addr: &str) -> String {
    let addr = addr.trim();
    if addr.is_empty() {
        return String::new();
    }
    // For `[::1]:4433` only the part after `]` can hold the port.
    let tail = match addr.rfind(']') {
        Some(i) => &addr[i + 1..],
        None => addr,
    };
    if tail.contains(':') {
        addr.to_string()
    } else {
        format!("{addr}:{DEFAULT_PORT}")
    }
}

enum SessionEnd {
    ServerClosed,
    Shutdown,
}

pub async fn run_client(
    server_addr: String,
    proxy: EventLoopProxy<UserEvent>,
    mut outgoing: UnboundedReceiver<ClientMessage>,
    tick_metrics: bool,
) {
    let mut last_reason: Option<String> = None;
    loop {
        let _ = proxy.send_event(UserEvent::Network(NetworkStatus::Connecting(
            last_reason.clone(),
        )));

        match run_session(&server_addr, &proxy, &mut outgoing, tick_metrics).await {
            Ok(SessionEnd::Shutdown) => return,
            Ok(SessionEnd::ServerClosed) => {
                last_reason = Some("server closed connection".to_string());
                warn!("server closed connection; will reconnect");
            }
            Err(e) => {
                last_reason = Some(format!("{e:#}"));
                warn!("connection error: {e:#}; will reconnect");
            }
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

async fn run_session(
    server_addr: &str,
    proxy: &EventLoopProxy<UserEvent>,
    outgoing: &mut UnboundedReceiver<ClientMessage>,
    tick_metrics: bool,
) -> Result<SessionEnd> {
    // Resolved per attempt rather than once at startup: the address is
    // user-supplied, so a typo has to surface as a retryable status instead
    // of killing the app, and a hostname whose record changes is picked up
    // without a restart.
    let resolved = resolve(server_addr).await?;

    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(make_client_config()?);

    let conn = endpoint.connect(resolved, "localhost")?.await?;
    info!(remote = %conn.remote_address(), "connected");

    let welcome = request(&conn, &ClientMessage::Hello).await?;
    let (
        world_chunks_x,
        world_chunks_y,
        paused,
        tick_hz,
        tick_rate_limited,
        tick,
        seed,
        sim_params,
        world_gen_params,
    ) = match welcome {
        ServerMessage::Welcome {
            world_chunks_x,
            world_chunks_y,
            paused,
            tick_hz,
            tick_rate_limited,
            tick,
            seed,
            sim_params,
            world_gen_params,
        } => (
            world_chunks_x,
            world_chunks_y,
            paused,
            tick_hz,
            tick_rate_limited,
            tick,
            seed,
            sim_params,
            world_gen_params,
        ),
        other => bail!("unexpected first message: {other:?}"),
    };
    let _ = proxy.send_event(UserEvent::Network(NetworkStatus::Connected {
        world_chunks_x,
        world_chunks_y,
        paused,
        tick_hz,
        tick_rate_limited,
        tick,
        seed,
        sim_params,
        world_gen_params,
    }));

    let batch = request(&conn, &ClientMessage::Subscribe).await?;
    match batch {
        ServerMessage::ChunkBatch { tick, chunks } => {
            info!(count = chunks.len(), tick, "received initial chunk batch");
            let _ = proxy.send_event(UserEvent::Chunks { tick, chunks });
        }
        other => bail!("expected ChunkBatch, got {other:?}"),
    }

    info!("session live; multiplexing outgoing + incoming");
    let mut last_tick_arrival: Option<Instant> = None;
    loop {
        tokio::select! {
            biased;
            outgoing_msg = outgoing.recv() => {
                match outgoing_msg {
                    Some(msg) => send_command(&conn, &msg).await?,
                    None => return Ok(SessionEnd::Shutdown),
                }
            }
            stream = conn.accept_uni() => {
                let mut recv = match stream {
                    Ok(r) => r,
                    Err(e) => {
                        debug!("accept_uni: {e}");
                        return Ok(SessionEnd::ServerClosed);
                    }
                };
                let read_start = if tick_metrics { Some(Instant::now()) } else { None };
                let buf = recv
                    .read_to_end(8 * 1024 * 1024)
                    .instrument(tracing::info_span!("read"))
                    .await?;
                let read_us = read_start
                    .map(|t| t.elapsed().as_micros() as u64)
                    .unwrap_or(0);
                let bytes = buf.len();
                let _ = proxy.send_event(UserEvent::WireBytes(bytes));
                let decode_start = if tick_metrics { Some(Instant::now()) } else { None };
                let msg = {
                    let _decode_span = tracing::info_span!("decode", bytes).entered();
                    protocol::decode_server_message(&buf)?
                };
                let decode_us = decode_start
                    .map(|t| t.elapsed().as_micros() as u64)
                    .unwrap_or(0);
                match msg {
                    ServerMessage::ChunkBatch { tick, chunks } => {
                        if tick_metrics {
                            let now = Instant::now();
                            let inter_arrival_us = last_tick_arrival
                                .map(|t| now.duration_since(t).as_micros() as u64)
                                .unwrap_or(0);
                            last_tick_arrival = Some(now);
                            info!(
                                tick,
                                bytes,
                                read_us,
                                decode_us,
                                inter_arrival_us,
                                "tick received (full)"
                            );
                        } else {
                            debug!(count = chunks.len(), tick, "tick chunk batch (full)");
                        }
                        let _ = proxy.send_event(UserEvent::Chunks { tick, chunks });
                    }
                    ServerMessage::ChunkDelta { tick, chunks } => {
                        if tick_metrics {
                            let now = Instant::now();
                            let inter_arrival_us = last_tick_arrival
                                .map(|t| now.duration_since(t).as_micros() as u64)
                                .unwrap_or(0);
                            last_tick_arrival = Some(now);
                            info!(
                                tick,
                                bytes,
                                read_us,
                                decode_us,
                                inter_arrival_us,
                                dirty = chunks.len(),
                                "tick received (delta)"
                            );
                        } else {
                            debug!(dirty = chunks.len(), tick, "tick chunk delta");
                        }
                        let _ = proxy.send_event(UserEvent::ChunksDelta { tick, chunks });
                    }
                    ServerMessage::Welcome {
                        world_chunks_x,
                        world_chunks_y,
                        paused,
                        tick_hz,
                        tick_rate_limited,
                        tick,
                        seed,
                        sim_params,
                        world_gen_params,
                    } => {
                        // Server pushes a fresh Welcome after a regenerate
                        // so connected viewers refresh seed/tick state.
                        debug!(seed, tick, "world regenerated");
                        let _ = proxy.send_event(UserEvent::Network(
                            NetworkStatus::Connected {
                                world_chunks_x,
                                world_chunks_y,
                                paused,
                                tick_hz,
                                tick_rate_limited,
                                tick,
                                seed,
                                sim_params,
                                world_gen_params,
                            },
                        ));
                    }
                }
            }
        }
    }
}

/// DNS (or literal-IP) resolution for a `host:port` string.
///
/// `tokio::net::lookup_host` runs the blocking resolver on a worker thread,
/// so this is safe to call from the connect loop.
async fn resolve(addr: &str) -> Result<SocketAddr> {
    tokio::net::lookup_host(addr)
        .await
        .with_context(|| format!("resolve {addr}"))?
        .next()
        .ok_or_else(|| anyhow!("no addresses for {addr}"))
}

async fn request(conn: &quinn::Connection, msg: &ClientMessage) -> Result<ServerMessage> {
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(&rmp_serde::to_vec(msg)?).await?;
    send.finish()?;
    let buf = recv.read_to_end(8 * 1024 * 1024).await?;
    Ok(protocol::decode_server_message(&buf)?)
}

async fn send_command(conn: &quinn::Connection, msg: &ClientMessage) -> Result<()> {
    let mut send = conn.open_uni().await?;
    send.write_all(&rmp_serde::to_vec(msg)?).await?;
    send.finish()?;
    Ok(())
}

fn make_client_config() -> Result<quinn::ClientConfig> {
    let crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
        .with_no_client_auth();
    let quic = quinn::crypto::rustls::QuicClientConfig::try_from(crypto)?;
    let mut config = quinn::ClientConfig::new(Arc::new(quic));

    let mut transport = quinn::TransportConfig::default();
    transport.keep_alive_interval(Some(KEEP_ALIVE));
    transport.max_idle_timeout(Some(IDLE_TIMEOUT.try_into()?));
    config.transport_config(Arc::new(transport));
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::with_default_port;

    #[test]
    fn bare_host_gets_default_port() {
        assert_eq!(with_default_port("example.com"), "example.com:4433");
        assert_eq!(with_default_port("192.168.0.10"), "192.168.0.10:4433");
    }

    #[test]
    fn explicit_port_is_preserved() {
        assert_eq!(with_default_port("example.com:1234"), "example.com:1234");
        assert_eq!(with_default_port("192.168.0.10:1234"), "192.168.0.10:1234");
    }

    #[test]
    fn bracketed_ipv6_is_handled() {
        assert_eq!(with_default_port("[::1]"), "[::1]:4433");
        assert_eq!(with_default_port("[::1]:1234"), "[::1]:1234");
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        assert_eq!(with_default_port("  example.com  "), "example.com:4433");
    }

    #[test]
    fn empty_stays_empty() {
        assert_eq!(with_default_port(""), "");
        assert_eq!(with_default_port("   "), "");
    }
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
