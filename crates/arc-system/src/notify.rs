//! Desktop notifications via org.freedesktop.Notifications (the Omarchy
//! shell implements it). Falls back to `notify-send` if the bus call fails.

use crate::{Result, SHORT, run};
use std::collections::HashMap;
use zbus::zvariant::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    Low = 0,
    Normal = 1,
    Critical = 2,
}

pub async fn send(summary: &str, body: &str, urgency: Urgency, timeout_ms: i32) -> Result<u32> {
    match send_dbus(summary, body, urgency, timeout_ms).await {
        Ok(id) => Ok(id),
        Err(e) => {
            tracing::debug!(error = %e, "notification over D-Bus failed; trying notify-send");
            let u = match urgency {
                Urgency::Low => "low",
                Urgency::Normal => "normal",
                Urgency::Critical => "critical",
            };
            let t = timeout_ms.to_string();
            let out =
                run("notify-send", &["-a", "Arc", "-u", u, "-t", &t, "-p", summary, body], SHORT).await?;
            Ok(out.trim().parse().unwrap_or(0))
        }
    }
}

async fn send_dbus(summary: &str, body: &str, urgency: Urgency, timeout_ms: i32) -> Result<u32> {
    let conn = zbus::Connection::session().await?;
    let proxy = zbus::Proxy::new(
        &conn,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
    )
    .await?;
    let mut hints: HashMap<&str, Value<'_>> = HashMap::new();
    hints.insert("urgency", Value::U8(urgency as u8));
    hints.insert("desktop-entry", Value::from("arc"));
    let actions: Vec<&str> = vec![];
    let id: u32 = proxy
        .call("Notify", &("Arc", 0u32, "audio-input-microphone", summary, body, actions, hints, timeout_ms))
        .await?;
    Ok(id)
}
