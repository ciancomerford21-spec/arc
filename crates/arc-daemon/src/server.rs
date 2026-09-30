//! Unix-socket server: newline-delimited JSON, one request → one response,
//! plus pushed events for subscribed connections.
//!
//! Security: the socket directory is 0700 and the socket 0600, and every
//! connection's peer UID is checked with `SO_PEERCRED` — only the user
//! running the daemon may connect.

use crate::state::Daemon;
use arc_proto::{
    ClientMessage, ErrorCode, Event, MemoryRequest, PROTOCOL_VERSION, Request, ServerMessage, Topic,
    VoiceCommand, encode_line,
};
use serde_json::json;
use std::collections::HashSet;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc};

/// Max bytes in one request line; longer lines close the connection.
const MAX_LINE: usize = 256 * 1024;

pub fn bind(path: &Path) -> std::io::Result<UnixListener> {
    let dir = path.parent().unwrap_or(Path::new("/"));
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    if path.exists() {
        // Refuse to steal the socket from a running daemon.
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                format!("another arcd is already listening on {}", path.display()),
            ));
        }
        std::fs::remove_file(path)?;
    }
    let l = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(l)
}

fn peer_uid(s: &UnixStream) -> Option<u32> {
    s.peer_cred().ok().map(|c| c.uid())
}

pub async fn serve(listener: UnixListener, daemon: Arc<Daemon>) {
    let me = unsafe { libc::getuid() };
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "accept failed");
                continue;
            }
        };
        match peer_uid(&stream) {
            Some(uid) if uid == me => {}
            other => {
                tracing::warn!(peer_uid = ?other, "rejected connection from another user");
                continue;
            }
        }
        let d = daemon.clone();
        tokio::spawn(async move {
            if let Err(e) = connection(stream, d).await {
                tracing::debug!(error = %e, "connection closed");
            }
        });
    }
}

async fn connection(stream: UnixStream, daemon: Arc<Daemon>) -> anyhow::Result<()> {
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd);
    // All writes go through one task so responses and events never interleave
    // mid-line.
    let (tx, mut rx) = mpsc::channel::<String>(256);
    let writer = tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            if wr.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });
    let mut sub_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut buf = String::new();
    loop {
        buf.clear();
        let n = AsyncReadExt::take(&mut lines, MAX_LINE as u64 + 1).read_line(&mut buf).await?;
        if n == 0 {
            break;
        }
        if buf.len() > MAX_LINE {
            let _ = tx
                .send(encode_line(&ServerMessage::err(0, ErrorCode::BadRequest, "request too large")))
                .await;
            break;
        }
        let line = buf.trim();
        if line.is_empty() {
            continue;
        }
        let msg: ClientMessage = match serde_json::from_str(line) {
            Ok(m) => m,
            Err(e) => {
                let id = serde_json::from_str::<serde_json::Value>(line)
                    .ok()
                    .and_then(|v| v.get("id").and_then(|i| i.as_u64()))
                    .unwrap_or(0);
                let _ =
                    tx.send(encode_line(&ServerMessage::err(id, ErrorCode::BadRequest, e.to_string()))).await;
                continue;
            }
        };
        if let Request::Subscribe { topics } = &msg.request {
            let topics: HashSet<Topic> = topics.iter().copied().collect();
            if let Some(t) = sub_task.take() {
                t.abort();
            }
            sub_task =
                Some(tokio::spawn(forward_events(daemon.events.subscribe(), topics.clone(), tx.clone())));
            let _ = tx.send(encode_line(&ServerMessage::ok(msg.id, json!({"subscribed": topics})))).await;
            continue;
        }
        // Handle requests concurrently so a slow `ask` doesn't block
        // `status` on the same connection.
        let d = daemon.clone();
        let tx2 = tx.clone();
        tokio::spawn(async move {
            let reply = handle(&d, msg).await;
            let _ = tx2.send(encode_line(&reply)).await;
        });
    }
    // A client that subscribed and then half-closed its side (e.g.
    // `printf '{"type":"subscribe",...}' | socat - UNIX:...`) still wants
    // events: keep forwarding until a write fails (client fully gone).
    if let Some(t) = sub_task {
        drop(tx);
        let _ = t.await;
        let _ = writer.await;
        return Ok(());
    }
    drop(tx);
    let _ = writer.await;
    Ok(())
}

async fn forward_events(
    mut rx: broadcast::Receiver<Event>,
    topics: HashSet<Topic>,
    tx: mpsc::Sender<String>,
) {
    loop {
        match rx.recv().await {
            Ok(e) => {
                if topics.contains(&e.topic())
                    && tx.send(encode_line(&ServerMessage::Event { event: e })).await.is_err()
                {
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(dropped = n, "slow subscriber; events dropped");
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

pub async fn handle(d: &Daemon, msg: ClientMessage) -> ServerMessage {
    let id = msg.id;
    match msg.request {
        Request::Ping => ServerMessage::ok(
            id,
            json!({"pong": true, "version": env!("CARGO_PKG_VERSION"), "protocol": PROTOCOL_VERSION}),
        ),
        Request::Hello { client, version } => {
            tracing::debug!(?client, %version, "client hello");
            ServerMessage::ok(id, json!({"protocol": PROTOCOL_VERSION}))
        }
        Request::Ask { text, source } => {
            let text = text.trim();
            if text.is_empty() {
                return ServerMessage::err(id, ErrorCode::BadRequest, "empty request");
            }
            let r = d.ask(text, source).await;
            let speak = (source == arc_proto::InputSource::Voice && d.config.voice.speak_replies)
                || (source != arc_proto::InputSource::Voice && d.config.voice.speak_text_replies);
            if speak && !r.reply.is_empty() {
                d.speak(&r.reply);
            }
            ServerMessage::ok(id, r)
        }
        Request::Confirm { confirmation_id, approve } => {
            ServerMessage::ok(id, d.confirm(&confirmation_id, approve).await)
        }
        Request::CallTool { tool, args } => ServerMessage::ok(id, d.call_tool(&tool, args).await),
        Request::NowPlaying(r) => match d.now_playing(r) {
            Ok(status) => ServerMessage::ok(id, status),
            Err(e) => ServerMessage::err(id, ErrorCode::BadRequest, e),
        },
        Request::Status => ServerMessage::ok(id, d.status()),
        Request::BarStatus => ServerMessage::ok(id, d.bar_status()),
        Request::Tools => ServerMessage::ok(id, d.tool_list()),
        Request::SetToolClass { tool, level } => match d.set_tool_class(&tool, level) {
            Ok(info) => ServerMessage::ok(id, info),
            Err(e) => ServerMessage::err(id, ErrorCode::NotFound, e),
        },
        Request::Voice { command } => {
            if matches!(command, VoiceCommand::Speak { .. } | VoiceCommand::StopSpeaking)
                || d.snapshot().voice.is_some()
            {
                d.emit(Event::VoiceControl { command });
                ServerMessage::ok(id, json!({"sent": true}))
            } else {
                ServerMessage::err(id, ErrorCode::Unavailable, "the voice service is not running")
            }
        }
        Request::ResetConversation => {
            d.assistant.clear_history();
            ServerMessage::ok(id, json!({"reset": true}))
        }
        Request::VoiceReport { .. } => ServerMessage::err(
            id,
            ErrorCode::BadRequest,
            "voice reports are accepted only from the daemon's own voice service",
        ),
        Request::Memory(m) => memory(d, id, m),
        Request::Desktop
        | Request::Automations(_)
        | Request::Permissions
        | Request::Audit { .. }
        | Request::Reload => ServerMessage::err(id, ErrorCode::Unavailable, "not implemented yet"),
        Request::Subscribe { .. } => unreachable!("handled by the connection loop"),
    }
}

/// `Request::Memory` operations. Facts live in the store attached to the
/// assistant; the daemon is the only writer, so clients go through here.
fn memory(d: &Daemon, id: u64, m: MemoryRequest) -> ServerMessage {
    let Some(store) = d.assistant.memory() else {
        return ServerMessage::err(id, ErrorCode::Unavailable, "memory is disabled");
    };
    match m {
        MemoryRequest::List { category } => {
            let entries = match &category {
                Some(c) => store.search(vec![c.as_str()]),
                None => store.list(),
            };
            let facts: Vec<serde_json::Value> = entries
                .iter()
                .map(|e| {
                    serde_json::json!({
                        "id": e.id,
                        "fact": e.fact,
                        "tags": e.tags,
                        "remembered_at": e.remembered_at,
                    })
                })
                .collect();
            ServerMessage::ok(id, json!({"count": facts.len(), "facts": facts}))
        }
        MemoryRequest::Remember { key, value, .. } => {
            // The proto's key/value pair is stored as one readable sentence so
            // it comes back out in the prompt the way it went in.
            let fact = format!("{key}: {value}");
            let e = store.remember(fact, vec![]);
            ServerMessage::ok(id, json!({"id": e.id, "fact": e.fact, "remembered_at": e.remembered_at}))
        }
        MemoryRequest::Forget { key } => {
            // Accept either the entry id or the leading `key` of a stored fact.
            if store.forget(&key) {
                return ServerMessage::ok(id, json!({"forgotten": key}));
            }
            match store.list().into_iter().find(|e| e.fact.starts_with(&format!("{key}:"))) {
                Some(e) if store.forget(&e.id) => ServerMessage::ok(id, json!({"forgotten": e.id})),
                _ => ServerMessage::err(id, ErrorCode::NotFound, format!("no fact matching `{key}`")),
            }
        }
        MemoryRequest::ForgetLast => match store.list().last() {
            Some(e) if store.forget(&e.id) => ServerMessage::ok(id, json!({"forgotten": e.fact})),
            _ => ServerMessage::err(id, ErrorCode::NotFound, "no facts to forget"),
        },
        MemoryRequest::Clear => {
            let n = store.clear();
            ServerMessage::ok(id, json!({"cleared": n}))
        }
    }
}
