mod app;
mod net;
mod render;

use std::{net::SocketAddr, sync::Arc};

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;
use winit::event_loop::EventLoop;

use crate::app::{App, UserEvent};

#[derive(Parser, Debug)]
#[command(version, about = "cellular-automata viewer")]
struct Args {
    /// Server address to connect to.
    #[arg(long, default_value = "127.0.0.1:4433")]
    server_addr: SocketAddr,
}

fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install default rustls crypto provider");

    let rt = Arc::new(tokio::runtime::Runtime::new()?);

    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();

    let mut app = App::new(rt, proxy, args.server_addr);
    event_loop.run_app(&mut app)?;
    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("info,quinn=warn,wgpu_core=warn,wgpu_hal=warn,naga=warn")
    });
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
