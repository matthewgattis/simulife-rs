mod app;
mod net;
mod render;

use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use anyhow::Result;
use clap::Parser;
use tokio::sync::mpsc;
use tracing::info;
use tracing_chrome::{ChromeLayerBuilder, FlushGuard};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};
use winit::event_loop::EventLoop;

use crate::app::{App, UserEvent};

#[derive(Parser, Debug)]
#[command(version, about = "cellular-automata viewer")]
struct Args {
    /// Server address to connect to.
    #[arg(long, default_value = "127.0.0.1:4433")]
    server_addr: SocketAddr,

    /// Log per-tick timing (read/decode/upload times). Off by default.
    #[arg(long)]
    tick_metrics: bool,

    /// If set, write a Chrome-trace JSON profile to this path. Open with
    /// `chrome://tracing` or https://ui.perfetto.dev.
    #[arg(long)]
    trace_chrome: Option<PathBuf>,

    /// Optional graceful exit after N seconds. Lets profiling runs flush
    /// trace data without manual intervention.
    #[arg(long)]
    profile_duration_secs: Option<u64>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let _trace_guard = init_tracing(args.trace_chrome.as_deref());

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install default rustls crypto provider");

    let rt = Arc::new(tokio::runtime::Runtime::new()?);

    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();

    if let Some(secs) = args.profile_duration_secs {
        let shutdown_proxy = proxy.clone();
        rt.spawn(async move {
            tokio::time::sleep(Duration::from_secs(secs)).await;
            info!(secs, "profile duration elapsed; exiting viewer");
            let _ = shutdown_proxy.send_event(UserEvent::Shutdown);
        });
    }

    let (outgoing_tx, outgoing_rx) = mpsc::unbounded_channel();

    let mut app = App::new(
        rt,
        proxy,
        args.server_addr,
        outgoing_tx,
        outgoing_rx,
        args.tick_metrics,
    );
    event_loop.run_app(&mut app)?;
    Ok(())
}

fn init_tracing(trace_chrome: Option<&std::path::Path>) -> Option<FlushGuard> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("info,quinn=warn,wgpu_core=warn,wgpu_hal=warn,naga=warn")
    });
    let fmt_layer = tracing_subscriber::fmt::layer();
    match trace_chrome {
        Some(path) => {
            let (chrome_layer, guard) = ChromeLayerBuilder::new()
                .file(path)
                .include_args(true)
                .build();
            tracing_subscriber::registry()
                .with(filter)
                .with(fmt_layer)
                .with(chrome_layer)
                .init();
            info!(path = %path.display(), "chrome trace recording");
            Some(guard)
        }
        None => {
            tracing_subscriber::registry()
                .with(filter)
                .with(fmt_layer)
                .init();
            None
        }
    }
}
