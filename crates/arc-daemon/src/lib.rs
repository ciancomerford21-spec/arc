//! The Arc daemon: socket server, assistant, voice supervisor.

pub mod music;
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
    tokio::spawn(supervise_music(daemon.clone()));
    shutdown_signal().await;
    tracing::info!("shutting down");
    srv.abort();
    daemon.shutdown_music();
    let _ = std::fs::remove_file(&opts.socket);
    if let Some(b) = &opts.bar_file {
        let _ = std::fs::remove_file(b);
    }
    Ok(())
}

/// How often the daemon checks on the player.
///
/// Once a second: enough that the overlay clears within a second of the queue
/// ending and a progress bar moves visibly, and it costs one socket round trip
/// per second on a machine that is already playing music. When nothing is
/// playing the tick does nothing at all -- no player call, no event.
const MUSIC_POLL: std::time::Duration = std::time::Duration::from_secs(1);

/// Follow the player: adopt whichever track it moved to, note the playhead,
/// and clear the music section when the queue runs out.
///
/// Poll rather than event stream on purpose. The player interleaves its own
/// events with its command replies on one socket, and reading them from here
/// would mean holding that socket -- which is exactly what a poll needs no
/// machinery for. A poll cannot miss an event that arrived unread.
async fn supervise_music(daemon: Arc<state::Daemon>) {
    loop {
        tokio::time::sleep(MUSIC_POLL).await;
        daemon.tick_music();
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
