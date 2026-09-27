//! App-launch helpers: mirrors `omarchy-launch-or-focus` so the caller can
//! reason about what happened, with a raw `hyprctl dispatch exec` fallback.

use crate::{Result, SHORT, run};

/// Launch or focus an app by class/title pattern.
pub async fn launch(pattern: &str, command: Option<&str>) -> Result<String> {
    let args: Vec<&str> = match command {
        Some(cmd) => vec![pattern, cmd],
        None => vec![pattern],
    };
    let out = run("omarchy-launch-or-focus", &args, SHORT).await;
    if let Ok(s) = out {
        return Ok(s);
    }
    let cmd = command.unwrap_or(pattern);
    let args2 = vec!["dispatch", "exec", "--", cmd];
    run("hyprctl", &args2, SHORT).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn launch_missing_command_is_reported_gracefully() {
        let r = launch("nonexistent-app-arc", None).await;
        assert!(r.is_err());
    }
}
