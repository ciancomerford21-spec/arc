//! The Arc daemon: socket server, assistant, voice supervisor.

pub mod server;
pub mod state;
pub mod voice;

use std::path::PathBuf;
use std::sync::Arc;

pub struct Options {
    pub socket: PathBuf,
    pub bar_file: Option<PathBuf>,
    pub voice: Option<voice::VoiceLaunch>,
}

/// Run until the process is signalled.
pub async fn run(config: arc_config::Config, opts: Options) -> anyhow::Result<()> {
    let daemon = Arc::new(state::Daemon::new(config, opts.bar_file.clone()).map_err(anyhow::Error::msg)?);
    let listener = server::bind(&opts.socket)?;
    tracing::info!(socket = %opts.socket.display(), provider = daemon.assistant.provider_name(), "arcd listening");
    daemon.write_bar();
    let srv = tokio::spawn(server::serve(listener, daemon.clone()));
    if let Some(v) = opts.voice {
        tokio::spawn(voice::supervise(daemon.clone(), v));
    }
    shutdown_signal().await;
    tracing::info!("shutting down");
    srv.abort();
    let _ = std::fs::remove_file(&opts.socket);
    if let Some(b) = &opts.bar_file {
        let _ = std::fs::remove_file(b);
    }
    Ok(())
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}
