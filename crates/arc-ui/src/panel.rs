// GTK4 layer-shell panel for Arc: a small always-available window showing
// what the assistant is doing, the live transcript, and Approve/Reject
// buttons for anything Arc is holding for confirmation.
//
// State comes from the same `bar.json` the Omarchy bar widget reads, so the
// two can never disagree. It is a few hundred bytes and rewritten on every
// state change, so polling it on a timer is cheaper than maintaining an
// inotify watch.
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use gtk4::glib;
use gtk4::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use arc_proto::BarStatus;

/// How often to re-read bar.json while the window is mapped.
const POLL_MS: u32 = 200;

/// The mutable half of the panel, owned by the widgets so the poll timer and
/// the button callbacks can reach it without borrowing the whole window.
#[derive(Default)]
struct PanelState {
    /// The confirmation id currently held by the daemon, if any.
    pending: Option<String>,
}

pub struct Panel {
    window: gtk4::ApplicationWindow,
    state_label: gtk4::Label,
    dot: gtk4::Label,
    transcript: gtk4::Label,
    reply: gtk4::Label,
    actions_row: gtk4::Box,
    confirm_ok: gtk4::Button,
    confirm_cancel: gtk4::Button,
    state: Rc<RefCell<PanelState>>,
    poll_id: RefCell<Option<glib::SourceId>>,
    /// Sends a confirmation over the daemon socket; set by `main`.
    on_confirm: Rc<dyn Fn(bool)>,
}

impl Panel {
    pub fn new(
        app: &gtk4::Application,
        bar_path: PathBuf,
        on_confirm: Rc<dyn Fn(bool)>,
    ) -> Self {
        let window = gtk4::ApplicationWindow::builder()
            .application(app)
            .title("Arc")
            .default_width(340)
            .default_height(240)
            .resizable(false)
            .build();
        init_layer_shell(&window);

        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 10);
        root.set_margin_start(14);
        root.set_margin_end(14);
        root.set_margin_top(14);
        root.set_margin_bottom(14);
        root.set_spacing(8);

        let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        let state_label = gtk4::Label::builder()
            .label("Arc")
            .xalign(0.0)
            .hexpand(true)
            .build();
        let dot = gtk4::Label::new(Some("●"));
        header.append(&state_label);
        header.append(&dot);
        root.append(&header);

        root.append(&header_separator());

        let transcript = body_label(gtk4::Align::Start, "arc-heard");
        let reply = body_label(gtk4::Align::End, "arc-spoke");
        root.append(&transcript);
        root.append(&reply);

        let actions_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        let confirm_ok = gtk4::Button::with_label("Approve");
        let confirm_cancel = gtk4::Button::with_label("Reject");
        confirm_ok.add_css_class("suggested-action");
        actions_row.append(&confirm_ok);
        actions_row.append(&confirm_cancel);
        actions_row.set_visible(false);
        root.append(&actions_row);

        install_theme(&window);
        window.set_child(Some(&root));

        let state = Rc::new(RefCell::new(PanelState::default()));
        let panel = Self {
            window,
            state_label,
            dot,
            transcript,
            reply,
            actions_row,
            confirm_ok,
            confirm_cancel,
            state: state.clone(),
            poll_id: RefCell::new(None),
            on_confirm,
        };
        panel.wire_buttons();
        panel.start_polling(&bar_path);
        panel
    }

    /// Both buttons report the same way: they send the held confirmation with
    /// the answer, then hide — the daemon will push a new state if there is
    /// still something outstanding.
    fn wire_buttons(&self) {
        for (button, approve) in [(&self.confirm_ok, true), (&self.confirm_cancel, false)] {
            let state = self.state.clone();
            let send = self.on_confirm.clone();
            let row = self.actions_row.clone();
            button.connect_clicked(move |_| {
                if state.borrow().pending.is_none() {
                    return;
                }
                send(approve);
                state.borrow_mut().pending = None;
                row.set_visible(false);
            });
        }
    }

    /// Poll only while visible: a hidden panel costs nothing, and the socket
    /// is not touched unless the user presses a button.
    fn start_polling(&self, bar_path: &Path) {
        self.stop_polling();

        let state_label = self.state_label.clone();
        let dot = self.dot.clone();
        let transcript = self.transcript.clone();
        let reply = self.reply.clone();
        let path = bar_path.to_path_buf();
        let mut last: Option<BarStatus> = None;

        let id = glib::timeout_add_local(Duration::from_millis(POLL_MS.into()), move || {
            let Some(status) = read_status(&path) else {
                return glib::ControlFlow::Continue;
            };
            // Re-applying identical status every 200ms would fight the user's
            // text selection, so only touch the widgets on a real change.
            if last.as_ref() == Some(&status) {
                return glib::ControlFlow::Continue;
            }
            let first = last.is_none();
            last = Some(status.clone());
            apply_status(&state_label, &dot, &transcript, &reply, &status, first);
            glib::ControlFlow::Continue
        });

        *self.poll_id.borrow_mut() = Some(id);

        self.window.connect_map(move |w| {
            w.set_visible(true);
        });
    }

    fn stop_polling(&self) {
        if let Some(id) = self.poll_id.borrow_mut().take() {
            id.remove();
        }
    }

    /// Show the panel and start polling. Called from the app's activate and
    /// from the window-map handler, so both paths are covered.
    pub fn present(&self) {
        self.window.present();
    }

    /// Record a confirmation the daemon is holding and reveal the buttons.
    pub fn set_pending(&self, id: Option<String>) {
        self.state.borrow_mut().pending = id.clone();
        self.actions_row.set_visible(id.is_some());
    }
}

/// Push one status onto the widgets. `first` suppresses the "Arc" placeholder
/// replacement so an empty reply is not written over a better one.
fn apply_status(
    state_label: &gtk4::Label,
    dot: &gtk4::Label,
    transcript: &gtk4::Label,
    reply: &gtk4::Label,
    status: &BarStatus,
    first: bool,
) {
    state_label.set_text(&status.state.label());
    if let Some(tooltip) = status.tooltip.lines().next() {
        state_label.set_tooltip_text(Some(tooltip));
    }
    paint_dot(dot, &status.class);
    // `text` is whatever Arc is currently doing: the spoken reply once it has
    // one, and an ellipsis while it is still thinking.
    let text = status.text.trim();
    if !text.is_empty() && (first || text != "…") {
        if status.state == arc_proto::AssistantState::Listening {
            transcript.set_text(text);
        } else {
            reply.set_text(text);
        }
    }
}

fn paint_dot(dot: &gtk4::Label, class: &str) {
    let colour = match class {
        "listening" => "#1f6feb",
        "thinking" => "#d29922",
        "executing" => "#8250df",
        "speaking" => "#238636",
        "error" => "#da3633",
        _ => "#3d4450",
    };
    let provider = gtk4::CssProvider::new();
    provider.load_from_data(&format!("label.arc-dot {{ background:{colour}; border-radius:6px; }}"));
    dot.style_context()
        .add_provider(&provider, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION);
}

fn header_separator() -> gtk4::Separator {
    let s = gtk4::Separator::new(gtk4::Orientation::Horizontal);
    s.add_css_class("arc-separator");
    s
}

fn body_label(align: gtk4::Align, css: &str) -> gtk4::Label {
    let l = gtk4::Label::new(Some(""));
    l.set_xalign(if align == gtk4::Align::End { 1.0 } else { 0.0 });
    l.set_wrap(true);
    l.set_wrap_mode(gtk4::pango::WrapMode::Word);
    l.set_max_width_chars(44);
    l.set_selectable(true);
    l.set_vexpand(true);
    l.add_css_class(css);
    l
}

fn read_status(path: &Path) -> Option<BarStatus> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(raw.trim()).ok()
}

/// Register the window as a layer-shell surface. On a non-Wayland session this
/// is a no-op and the window is an ordinary one, which is fine for testing.
fn init_layer_shell(window: &gtk4::ApplicationWindow) {
    if !gtk4_layer_shell::is_supported() {
        tracing::warn!("layer-shell not supported (not Wayland?); using a plain window");
        return;
    }
    window.init_layer_shell();
    window.set_layer(Layer::Top);
    window.set_anchor(Edge::Top, true);
    window.set_anchor(Edge::Right, true);
    window.set_margin(Edge::Top, 40);
    window.set_margin(Edge::Right, 12);
    // Reserve space so the panel never covers the bar or a window edge.
    window.auto_exclusive_zone_enable();
    // No keyboard until a confirmation needs one; the voice path is primary.
    window.set_keyboard_mode(KeyboardMode::None);
    window.set_namespace(Some("arc-panel"));
}

/// Panel styling. Deliberately close to plain GTK defaults so it inherits the
/// desktop theme; only the pieces that have no sensible default are set here.
fn install_theme(window: &gtk4::ApplicationWindow) {
    let css = "\
window.arc-panel { background: alpha(@theme_bg_color, 0.92); border-radius: 12px; }\
label.arc-dot { font-size: 13px; }\
label.arc-heard { color: @dim_label_color; }\
label.arc-spoke { color: @theme_fg_color; }\
.arc-separator { margin: 2px 0; }\
button.suggested-action { font-weight: bold; }\
";
    let provider = gtk4::CssProvider::new();
    provider.load_from_data(css);
    window.style_context().add_provider(&provider, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION);
    window.add_css_class("arc-panel");
}
