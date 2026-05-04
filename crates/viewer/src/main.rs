use std::{net::SocketAddr, path::PathBuf, process, time::Duration};

use anyhow::Result;
use clap::Parser;
use tracing::info;
use tracing_chrome::{ChromeLayerBuilder, FlushGuard};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

use viewer::{RunOptions, run_viewer};

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

    run_viewer(RunOptions {
        server_addr: args.server_addr,
        tick_metrics: args.tick_metrics,
        profile_duration: args.profile_duration_secs.map(Duration::from_secs),
    })?;

    // On Linux, deferring the QUIC endpoint's background UDP task teardown
    // until normal process exit races with tokio runtime drop and segfaults.
    // A hard exit here is safe — all important state (trace flush guard, etc.)
    // lives on the stack above us.
    process::exit(0);
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
