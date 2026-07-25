mod app;
mod config;
mod net;
mod render;

use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::info;
use winit::event_loop::EventLoop;

use crate::app::{App, UserEvent};
use crate::config::ServerHistory;

pub struct RunOptions {
    /// Explicit address from the command line. Takes precedence over the
    /// saved history.
    pub server_addr: Option<String>,
    /// Used when neither the command line nor the history supplies an
    /// address. `None` means "ask the user" — the viewer opens the connect
    /// dialog instead of connecting to anything.
    pub fallback_addr: Option<String>,
    /// Where to persist the list of servers that connected successfully.
    /// `None` disables persistence for the session.
    pub config_path: Option<PathBuf>,
    pub tick_metrics: bool,
    pub profile_duration: Option<Duration>,
}

pub fn run_viewer(opts: RunOptions) -> Result<()> {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    run_with_event_loop(opts, event_loop)
}

fn run_with_event_loop(opts: RunOptions, event_loop: EventLoop<UserEvent>) -> Result<()> {
    // ok(): on Android, android_main may be re-entered after the OS rebuilds
    // the activity, in which case the provider is already installed.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let rt = Arc::new(tokio::runtime::Runtime::new()?);
    let proxy = event_loop.create_proxy();

    if let Some(d) = opts.profile_duration {
        let shutdown_proxy = proxy.clone();
        rt.spawn(async move {
            tokio::time::sleep(d).await;
            info!(
                secs = d.as_secs(),
                "profile duration elapsed; exiting viewer"
            );
            let _ = shutdown_proxy.send_event(UserEvent::Shutdown);
        });
    }

    let (outgoing_tx, outgoing_rx) = mpsc::unbounded_channel();

    let history = ServerHistory::load(opts.config_path);
    let server_addr = opts
        .server_addr
        .or_else(|| history.last().map(str::to_string))
        .or(opts.fallback_addr);

    let mut app = App::new(
        rt.clone(),
        proxy,
        server_addr,
        history,
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

#[cfg(target_os = "android")]
mod android {
    use android_activity::AndroidApp;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use winit::event_loop::EventLoop;
    use winit::platform::android::EventLoopBuilderExtAndroid;

    use crate::app::UserEvent;
    use crate::{RunOptions, run_with_event_loop};

    #[unsafe(no_mangle)]
    fn android_main(app: AndroidApp) {
        init_logging();

        // Servers are remembered in the app's private data dir. On a fresh
        // install there is nothing saved and no fallback, so the viewer opens
        // the connect dialog rather than dialling a compile-time constant.
        let config_path = app.internal_data_path().map(|p| p.join("servers.toml"));

        let event_loop = match EventLoop::<UserEvent>::with_user_event()
            .with_android_app(app)
            .build()
        {
            Ok(el) => el,
            Err(e) => {
                tracing::error!("build event loop: {e:#}");
                return;
            }
        };

        let opts = RunOptions {
            server_addr: None,
            fallback_addr: None,
            config_path,
            tick_metrics: false,
            profile_duration: None,
        };

        if let Err(e) = run_with_event_loop(opts, event_loop) {
            tracing::error!("viewer exited with error: {e:#}");
        }
    }

    fn init_logging() {
        // android_logger receives `log` records and ships them to logcat.
        // try_init keeps re-entry of android_main from panicking on the
        // second call (init_once handles its own re-entry, but the tracing
        // subscriber below also has to be guarded).
        android_logger::init_once(
            android_logger::Config::default()
                .with_max_level(log::LevelFilter::Info)
                .with_tag("CAViewer"),
        );

        let filter = tracing_subscriber::EnvFilter::new(
            "info,quinn=warn,wgpu_core=warn,wgpu_hal=warn,naga=warn",
        );
        let fmt_layer = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(AndroidLogWriter);
        let _ = tracing_subscriber::registry()
            .with(filter)
            .with(fmt_layer)
            .try_init();
    }

    /// Bridges tracing-subscriber's fmt layer into the `log` facade so each
    /// formatted line lands in logcat. Level info is flattened to `info!`;
    /// fine for development, revisit if we want per-event levels in logcat.
    struct AndroidLogWriter;

    impl std::io::Write for AndroidLogWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if let Ok(s) = std::str::from_utf8(buf) {
                let s = s.trim_end();
                if !s.is_empty() {
                    log::info!(target: "viewer", "{}", s);
                }
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for AndroidLogWriter {
        type Writer = AndroidLogWriter;
        fn make_writer(&'a self) -> Self::Writer {
            AndroidLogWriter
        }
    }
}
