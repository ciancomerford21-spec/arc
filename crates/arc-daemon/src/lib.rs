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
    tokio::spawn(reap_now_playing(daemon.clone()));
    shutdown_signal().await;
    tracing::info!("shutting down");
    srv.abort();
    let _ = std::fs::remove_file(&opts.socket);
    if let Some(b) = &opts.bar_file {
        let _ = std::fs::remove_file(b);
    }
    Ok(())
}

/// How often the player process is checked for liveness.
///
/// Fast enough that a finished track clears within a second of ending, slow
/// enough to be one `kill -0` per second on an idle system. The check is a
/// single syscall and nothing else, so this is not worth optimising.
const NOW_PLAYING_POLL: std::time::Duration = std::time::Duration::from_secs(1);

/// Clear the now-playing row when the player process goes away.
///
/// The playback tool starts mpv in the background and exits, so the daemon
/// never gets an "it finished" message -- the process disappearing is the only
/// signal there is. Without this the overlay would keep showing a track that
/// ended minutes ago.
async fn reap_now_playing(daemon: Arc<state::Daemon>) {
    loop {
        tokio::time::sleep(NOW_PLAYING_POLL).await;
        daemon.reap_now_playing();
    }
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
