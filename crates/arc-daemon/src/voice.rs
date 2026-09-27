//! Voice service supervisor.
//!
//! Spawns `python -m arc_voice` as a child, forwards `VoiceControl` events to
//! its stdin, and turns its stdout reports into daemon actions:
//! a `transcript` becomes an `ask` with `source = voice`, whose reply is
//! spoken back. The child is restarted with exponential backoff if it exits;
//! a `reload` command makes it exit on purpose, which restarts it with the
//! new configuration.

use crate::state::{Daemon, VoiceStatus};
use arc_proto::{
    AssistantState, ComponentHealth, ComponentStatus, Event, InputSource, VoiceCommand, VoiceReport, encode_line,
};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::broadcast;

#[derive(Debug, Clone)]
pub struct VoiceLaunch {
    pub python: PathBuf,
    /// Directory containing the `arc_voice` package (added to PYTHONPATH).
    pub package_dir: Option<PathBuf>,
    pub extra_args: Vec<String>,
}

impl VoiceLaunch {
    /// Defaults: the venv under the data dir, and the package next to the
    /// binary's source tree when running from a checkout.
    pub fn discover() -> Self {
        let data = arc_config::paths::data_dir();
        let python = std::env::var_os("ARC_VOICE_PYTHON")
            .map(PathBuf::from)
            .unwrap_or_else(|| data.join("venv/bin/python"));
        let package_dir = std::env::var_os("ARC_VOICE_PATH").map(PathBuf::from).or_else(|| {
            [data.join("python"), PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../python")]
                .into_iter()
                .find(|p| p.join("arc_voice/__init__.py").exists())
        });
        Self { python, package_dir, extra_args: vec![] }
    }

    fn command(&self) -> Command {
        let mut c = Command::new(&self.python);
        c.arg("-m").arg("arc_voice").args(&self.extra_args);
        if let Some(p) = &self.package_dir {
            c.env("PYTHONPATH", p);
        }
        c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        c
    }
}

fn health_list(r: &VoiceReport) -> Option<VoiceStatus> {
    let VoiceReport::Health { mic, vad, wake, stt, tts, mode, .. } = r else { return None };
    let c = |name: &str, h: &ComponentHealth| ComponentStatus { name: name.into(), status: h.status, detail: h.detail.clone() };
    Some(VoiceStatus {
        mode: *mode,
        components: vec![c("mic", mic), c("vad", vad), c("wake", wake), c("stt", stt), c("tts", tts)],
        running: true,
    })
}

/// Run the supervisor forever.
pub async fn supervise(daemon: Arc<Daemon>, launch: VoiceLaunch) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let started = Instant::now();
        match run_once(&daemon, &launch).await {
            Ok(Exit::Reload) => {
                tracing::info!("voice service restarting to reload configuration");
                backoff = Duration::from_secs(1);
                continue;
            }
            Ok(Exit::Exited(code)) => {
                tracing::warn!(?code, "voice service exited");
                daemon.record_error("voice", &format!("voice service exited ({code:?})"));
            }
            Err(e) => {
                tracing::error!(error = %e, python = %launch.python.display(), "cannot start voice service");
                daemon.record_error("voice", &format!("cannot start voice service: {e}"));
            }
        }
        daemon.set_voice(None);
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

enum Exit {
    Reload,
    Exited(Option<i32>),
}

async fn run_once(daemon: &Arc<Daemon>, launch: &VoiceLaunch) -> std::io::Result<Exit> {
    let mut child: Child = launch.command().spawn()?;
    tracing::info!(pid = ?child.id(), "voice service started");
    let mut stdin = child.stdin.take().expect("piped");
    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");

    // stderr -> our log
    tokio::spawn(async move {
        let mut l = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = l.next_line().await {
            tracing::info!(target: "arc_voice", "{line}");
        }
    });

    let mut control = daemon.events.subscribe();
    let mut reports = BufReader::new(stdout).lines();
    let mut reload = false;
    loop {
        tokio::select! {
            line = reports.next_line() => {
                match line {
                    Ok(Some(l)) => on_report(daemon, &l),
                    _ => break,
                }
            }
            ev = control.recv() => {
                let cmd = match ev {
                    Ok(Event::VoiceControl { command }) => command,
                    Ok(_) => continue,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                if cmd == VoiceCommand::Reload {
                    reload = true;
                }
                if stdin.write_all(encode_line(&cmd).as_bytes()).await.is_err() {
                    break;
                }
            }
        }
    }
    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    let code = match status {
        Ok(Ok(s)) => s.code(),
        _ => {
            let _ = child.kill().await;
            None
        }
    };
    Ok(if reload { Exit::Reload } else { Exit::Exited(code) })
}

fn on_report(daemon: &Arc<Daemon>, line: &str) {
    let report: VoiceReport = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, line, "bad report from voice service");
            return;
        }
    };
    if let Some(v) = health_list(&report) {
        daemon.set_voice(Some(v));
        return;
    }
    match report {
        VoiceReport::WakeDetected { keyword } => {
            tracing::info!(%keyword, "wake word");
        }
        VoiceReport::ListeningStarted => daemon.set_state(AssistantState::Listening, "listening"),
        VoiceReport::Transcribing => daemon.set_state(AssistantState::Thinking, "transcribing"),
        VoiceReport::NoSpeech => daemon.set_state(AssistantState::Idle, ""),
        VoiceReport::Level { rms } => daemon.emit(Event::VoiceLevel { rms }),
        VoiceReport::Transcript { text, stt_ms, audio_ms } => {
            tracing::info!(%text, stt_ms, audio_ms, "transcript");
            let d = daemon.clone();
            tokio::spawn(async move {
                let r = d.ask(&text, InputSource::Voice).await;
                if d.config.voice.speak_replies && !r.reply.is_empty() {
                    d.speak(&r.reply);
                }
            });
        }
        VoiceReport::SpeakingStarted { .. } => daemon.set_state(AssistantState::Speaking, "speaking"),
        VoiceReport::SpeakingFinished { .. } => daemon.set_state(AssistantState::Idle, ""),
        VoiceReport::Error { component, message } => daemon.record_error(&format!("voice.{component}"), &message),
        VoiceReport::Health { .. } => unreachable!(),
    }
}
