mod app;
mod net;
mod render;

use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::info;
use winit::event_loop::EventLoop;

use crate::app::{App, UserEvent};

pub struct RunOptions {
    pub server_addr: SocketAddr,
    pub tick_metrics: bool,
    pub profile_duration: Option<Duration>,
}

pub fn run_viewer(opts: RunOptions) -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install default rustls crypto provider");

    let rt = Arc::new(tokio::runtime::Runtime::new()?);

    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();

    if let Some(d) = opts.profile_duration {
        let shutdown_proxy = proxy.clone();
        rt.spawn(async move {
            tokio::time::sleep(d).await;
            info!(secs = d.as_secs(), "profile duration elapsed; exiting viewer");
            let _ = shutdown_proxy.send_event(UserEvent::Shutdown);
        });
    }

    let (outgoing_tx, outgoing_rx) = mpsc::unbounded_channel();

    let mut app = App::new(
        rt.clone(),
        proxy,
        opts.server_addr,
        outgoing_tx,
        outgoing_rx,
        opts.tick_metrics,
    );
    event_loop.run_app(&mut app)?;

    // GPU resources were released in `App::exiting()` while the display
    // connection was still live. The caller is responsible for any
    // platform-specific teardown workarounds (e.g. desktop's process::exit
    // to dodge the QUIC/tokio drop race on Linux).
    drop(app);
    drop(rt);
    Ok(())
}
