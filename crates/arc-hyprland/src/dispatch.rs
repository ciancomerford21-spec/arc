//! Typed dispatchers rendered to Hyprland's Lua dispatcher syntax.
//!
//! Syntax verified against Hyprland 0.56 on this machine (see
//! `tests/live_hyprland.rs`), e.g.
//! `hl.dsp.window.move({ workspace = "3", follow = false, window = "address:0x…" })`.
//! All strings are escaped as Lua string literals, so window titles or
//! workspace names can never inject Lua code.

use std::fmt::Write;

/// Which window an action applies to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowSel {
    /// The focused window at the time Hyprland executes the dispatcher.
    Active,
    /// Exact window, e.g. `0x6073182392c0` (with or without `0x`).
    Address(String),
}

impl WindowSel {
    fn selector(&self) -> Option<String> {
        match self {
            WindowSel::Active => None,
            WindowSel::Address(a) => {
                let a = a.trim().trim_start_matches("address:");
                let a = if a.starts_with("0x") { a.to_string() } else { format!("0x{a}") };
                Some(format!("address:{a}"))
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloatAction {
    Enable,
    Disable,
    Toggle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullscreenMode {
    Fullscreen,
    Maximized,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Dispatch {
    FocusWorkspace(String),
    FocusWindow(WindowSel),
    FocusMonitor(String),
    /// `focus({ direction = "l|r|u|d" })`
    FocusDirection(char),
    MoveToWorkspace {
        window: WindowSel,
        workspace: String,
        follow: bool,
    },
    MoveTo {
        window: WindowSel,
        x: i32,
        y: i32,
    },
    Resize {
        window: WindowSel,
        x: i32,
        y: i32,
        relative: bool,
    },
    Center(WindowSel),
    Float {
        window: WindowSel,
        action: FloatAction,
    },
    Fullscreen {
        window: WindowSel,
        mode: FullscreenMode,
    },
    Pin(WindowSel),
    Close(WindowSel),
    Swap(char),
    ToggleSpecial(String),
    MoveWorkspaceToMonitor(String),
    /// Launch a command. With `workspace`, it opens there (append " silent"
    /// to avoid switching).
    Exec {
        cmd: String,
        workspace: Option<String>,
    },
}

/// Quote as a Lua string literal.
pub fn lua_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                let _ = write!(out, "\\{}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn dir(c: char) -> &'static str {
    match c {
        'l' | 'L' => "l",
        'r' | 'R' => "r",
        'u' | 'U' => "u",
        _ => "d",
    }
}

/// Build `{ k = v, …, window = "address:…" }`.
fn table(fields: &[(&str, String)], window: &WindowSel) -> String {
    let mut parts: Vec<String> = fields.iter().map(|(k, v)| format!("{k} = {v}")).collect();
    if let Some(sel) = window.selector() {
        parts.push(format!("window = {}", lua_str(&sel)));
    }
    if parts.is_empty() { String::new() } else { format!("{{ {} }}", parts.join(", ")) }
}

impl Dispatch {
    pub fn to_lua(&self) -> String {
        use Dispatch::*;
        match self {
            FocusWorkspace(ws) => format!("hl.dsp.focus({{ workspace = {} }})", lua_str(ws)),
            FocusWindow(w) => match w.selector() {
                Some(sel) => format!("hl.dsp.focus({{ window = {} }})", lua_str(&sel)),
                None => "hl.dsp.no_op()".into(),
            },
            FocusMonitor(m) => format!("hl.dsp.focus({{ monitor = {} }})", lua_str(m)),
            FocusDirection(d) => format!("hl.dsp.focus({{ direction = \"{}\" }})", dir(*d)),
            MoveToWorkspace { window, workspace, follow } => format!(
                "hl.dsp.window.move({})",
                table(&[("workspace", lua_str(workspace)), ("follow", follow.to_string())], window)
            ),
            MoveTo { window, x, y } => {
                format!(
                    "hl.dsp.window.move({})",
                    table(&[("x", x.to_string()), ("y", y.to_string())], window)
                )
            }
            Resize { window, x, y, relative } => format!(
                "hl.dsp.window.resize({})",
                table(
                    &[("x", x.to_string()), ("y", y.to_string()), ("relative", relative.to_string())],
                    window
                )
            ),
            Center(w) => format!("hl.dsp.window.center({})", table(&[], w)),
            Float { window, action } => {
                let a = match action {
                    FloatAction::Enable => "enable",
                    FloatAction::Disable => "disable",
                    FloatAction::Toggle => "toggle",
                };
                format!("hl.dsp.window.float({})", table(&[("action", lua_str(a))], window))
            }
            Fullscreen { window, mode } => {
                let m = match mode {
                    FullscreenMode::Fullscreen => "fullscreen",
                    FullscreenMode::Maximized => "maximized",
                };
                format!("hl.dsp.window.fullscreen({})", table(&[("mode", lua_str(m))], window))
            }
            Pin(w) => format!("hl.dsp.window.pin({})", table(&[], w)),
            Close(w) => format!("hl.dsp.window.close({})", table(&[], w)),
            Swap(d) => format!("hl.dsp.window.swap({{ direction = \"{}\" }})", dir(*d)),
            ToggleSpecial(name) => format!("hl.dsp.workspace.toggle_special({})", lua_str(name)),
            MoveWorkspaceToMonitor(m) => format!("hl.dsp.workspace.move({{ monitor = {} }})", lua_str(m)),
            Exec { cmd, workspace } => match workspace {
                Some(ws) => format!("hl.dsp.exec_cmd({}, {{ workspace = {} }})", lua_str(cmd), lua_str(ws)),
                None => format!("hl.dsp.exec_cmd({})", lua_str(cmd)),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a() -> WindowSel {
        WindowSel::Address("6073182392c0".into())
    }

    #[test]
    fn renders_known_good_syntax() {
        assert_eq!(Dispatch::FocusWorkspace("3".into()).to_lua(), r#"hl.dsp.focus({ workspace = "3" })"#);
        assert_eq!(
            Dispatch::MoveToWorkspace { window: a(), workspace: "4".into(), follow: false }.to_lua(),
            r#"hl.dsp.window.move({ workspace = "4", follow = false, window = "address:0x6073182392c0" })"#
        );
        assert_eq!(
            Dispatch::Resize { window: a(), x: 640, y: 400, relative: false }.to_lua(),
            r#"hl.dsp.window.resize({ x = 640, y = 400, relative = false, window = "address:0x6073182392c0" })"#
        );
        assert_eq!(
            Dispatch::Float { window: WindowSel::Active, action: FloatAction::Toggle }.to_lua(),
            r#"hl.dsp.window.float({ action = "toggle" })"#
        );
        assert_eq!(Dispatch::Close(WindowSel::Active).to_lua(), "hl.dsp.window.close()");
        assert_eq!(
            Dispatch::Close(WindowSel::Address("address:0xabc".into())).to_lua(),
            r#"hl.dsp.window.close({ window = "address:0xabc" })"#
        );
        assert_eq!(
            Dispatch::Exec { cmd: "kitty".into(), workspace: Some("2 silent".into()) }.to_lua(),
            r#"hl.dsp.exec_cmd("kitty", { workspace = "2 silent" })"#
        );
        assert_eq!(Dispatch::FocusDirection('L').to_lua(), r#"hl.dsp.focus({ direction = "l" })"#);
    }

    #[test]
    fn strings_cannot_escape_the_literal() {
        let evil = "x\" }) hl.dsp.exit() --\n";
        let lua = Dispatch::FocusWorkspace(evil.into()).to_lua();
        assert_eq!(lua, r#"hl.dsp.focus({ workspace = "x\" }) hl.dsp.exit() --\n" })"#);
        assert_eq!(lua_str("a\\b"), r#""a\\b""#);
        assert_eq!(lua_str("\u{1}"), r#""\1""#);
    }
}
