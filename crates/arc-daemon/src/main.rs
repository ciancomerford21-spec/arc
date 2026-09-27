use clap::Parser;
use std::path::PathBuf;

/// Arc daemon.
#[derive(Parser)]
#[command(name = "arcd", version)]
struct Cli {
    /// Socket path (default: $XDG_RUNTIME_DIR/arc/arc.sock).
    #[arg(long, env = "ARC_SOCKET")]
    socket: Option<PathBuf>,
    /// Config file (default: ~/.config/arc/config.toml).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Don't start the voice service.
    #[arg(long)]
    no_voice: bool,
    /// Start the voice service without microphone/speaker (testing).
    #[arg(long)]
    voice_no_audio: bool,
    /// Don't write the bar status file.
    #[arg(long)]
    no_bar: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let loaded = match &cli.config {
        Some(p) => arc_config::load_from(p),
        None => arc_config::load(),
    }?;
    let filter = std::env::var("ARC_LOG").unwrap_or_else(|_| loaded.config.logging.level.clone());
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_new(&filter).unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
    for w in &loaded.warnings {
        tracing::warn!("config: {w}");
    }
    if loaded.defaulted {
        tracing::info!(path = %loaded.path.display(), "no config file; using defaults");
    }
    let voice = (!cli.no_voice && loaded.config.voice.enabled).then(|| {
        let mut v = arc_daemon::voice::VoiceLaunch::discover();
        if cli.voice_no_audio {
            v.extra_args.push("--no-audio".into());
        }
        // The voice service must read the same config file as the daemon.
        v.extra_args.push("--config".into());
        v.extra_args.push(loaded.path.display().to_string());
        v
    });
    let opts = arc_daemon::Options {
        socket: cli.socket.unwrap_or_else(arc_proto::socket_path),
        bar_file: (!cli.no_bar && loaded.config.bar.enabled).then(arc_config::paths::bar_status_file),
        voice,
    };
    arc_daemon::run(loaded.config, opts).await
}
