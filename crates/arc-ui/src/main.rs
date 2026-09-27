//! Entry point for the Arc panel: a GTK4 layer-shell window that shows what
//! the assistant is doing and lets the user approve or reject anything it is
//! holding.
//!
//! It reads `$XDG_RUNTIME_DIR/arc/bar.json` for state (the same file the
//! Omarchy bar widget reads) and sends confirmation over the existing daemon
//! socket. It never contacts a model and never runs a tool itself.
use std::rc::Rc;

use gtk4::prelude::*;

use arc_ui::Panel;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let bar_path = arc_config::paths::bar_status_file();
    let socket = arc_proto::socket_path();
    tracing::info!(bar = %bar_path.display(), socket = %socket.display(), "arc-ui starting");

    let app = gtk4::Application::builder().application_id("dev.ciancore.arc-panel").build();

    // Confirmation is a rare, deliberate click, so a short blocking request on
    // a worker thread beats pulling in an async runtime for one call.
    let on_confirm: Rc<dyn Fn(bool)> = Rc::new(move |approve: bool| {
        let sock = socket.clone();
        std::thread::spawn(move || {
            if let Err(e) = send_confirmation(&sock, approve) {
                tracing::error!(error = %e, "confirmation failed");
            }
        });
    });

    app.connect_activate(move |app| {
        let panel = Panel::new(app, bar_path.clone(), on_confirm.clone());
        panel.present();
    });
    app.run();
    Ok(())
}

/// Answer the confirmation the daemon is currently holding.
///
/// The daemon has no "confirm the latest" request, so this is the same path
/// `arc confirm` takes: ask with `yes`/`no` from the `ui` source, which the
/// daemon resolves against its own pending action.
fn send_confirmation(socket: &std::path::Path, approve: bool) -> Result<(), String> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let body = serde_json::json!({
        "type": "ask",
        "text": if approve { "yes" } else { "no" },
        "source": "ui",
    });
    let mut stream = UnixStream::connect(socket).map_err(|e| e.to_string())?;
    stream
        .write_all(format!("{body}\n").as_bytes())
        .map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut reply = String::new();
    reader.read_line(&mut reply).map_err(|e| e.to_string())?;
    tracing::info!(response = %reply.trim(), "confirmation answered");
    Ok(())
}
