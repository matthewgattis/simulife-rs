mod app;
mod net;
mod render;

use std::sync::Arc;

use anyhow::Result;
use winit::event_loop::EventLoop;

use crate::app::{App, UserEvent};

fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install default rustls crypto provider");

    let rt = Arc::new(tokio::runtime::Runtime::new()?);

    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();

    let mut app = App::new(rt, proxy);
    event_loop.run_app(&mut app)?;
    Ok(())
}
