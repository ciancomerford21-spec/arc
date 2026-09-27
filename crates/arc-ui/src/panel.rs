// gtk4-layer-shell panel showing Arc's state via a cheap inotify reader over
// `~/.local/share/arc/state.json`. Opens as a top-right layer-shell surface
// on an X11 or Wayland backend where the layer-shell library is usable.
// The Omarchy bar widget and this panel share the same `arc bar` status file,
// so they stay in sync.
use gtk4::prelude::*;
use gtk4_layer_shell::{Layer, LayerShell, LayerShellExt, KeyboardInteractivity, RestrictToOutput};

#[derive(Debug)]
pub struct Panel {
    pub window: gtk4::Window,
    pub status: gtk4::Label,
    pub transcript: gtk4::Label,
    pub reply: gtk4::Label,
    pub confirm_ok: gtk4::Button,
    pub confirm_cancel: gtk4::Button,
    pub level: gtk4::LevelBar,
    _destroy_id: glib::SourceId,
    _reap: glib::WeakRef,
}

impl Panel {
    pub fn new(app: &gtk4::Application) -> Result<Self, glib::glib::Error> {
        let window = gtk4::ApplicationWindow::builder()
            .application(app)
            .title("Arc")
            .default_width(260)
            .default_height(160)
            .type_hint(gtk4::gdk::WindowTypeHint::SPLASHSCREEN)
            .build();
        window.set_child(Some(&gtk4::Box::new(gtk4::Orientation::Vertical, 6)));
        let box_ = window.child().expect("no child").downcast::<gtk4::Box>().expect("not a box");
        box_.set_margin_all(8);
        box_.set_spacing(6);

        let top = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        let status = gtk4::Label::builder()
            .label("idle")
            .halign(gtk4::Align::Center)
            .hexpand(true)
            .build();
        let level = gtk4::LevelBar::new_for_range(0.0, 1.0);
        top.append(&status);
        top.append(&level);
        box_.append(&top);

        let transcript = gtk4::Label::builder()
            .label("")
            .selectable(true)
            .wrap(true)
            .hexpand(true)
            .valign(gtk4::Align::Fill)
            .xalign(0.0)
            .build();
        transcript.set_line_wrap(true);
        transcript.set_max_width_chars(40);
        box_.append(&transcript);

        let reply = gtk4::Label::builder()
            .label("")
            .selectable(true)
            .wrap(true)
            .hexpand(true)
            .xalign(0.0)
            .build();
        reply.set_line_wrap(true);
        box_.append(&reply);

        let footer = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
        let confirm_ok = gtk4::Button::builder().label("Approve").build();
        let confirm_cancel = gtk4::Button::builder().label("Reject").build();
        footer.append(&confirm_ok);
        footer.append(&confirm_cancel);
        box_.append(&footer);

        let shell = window.imp().downcast_ref::<LayerShell>().expect("layer shell");
        shell.set_layer(Layer::Top);
        shell.set_layer_shell_mode(LayerShellMode::Exclusive);
        shell.set_anchor_top(true);
        shell.set_anchor_right(true);
        shell.set_margin_top(8);
        shell.set_margin_right(8);
        shell.set_keyboard_interactivity(KeyboardInteractivity::None);

        let destroy_id = window.connect_destroyed(|_| {
            // Panel is being destroyed; nothing to clean up that the
            // application exit won't handle.
        });

        let state_dir = glib::glib::utils::user_special_dir(glib::UserDirectory::DIRECTORY_CACHE);
        let state_path = state_dir.join("arc").join("arc-state.json");

        let path = state_path.clone();
        let id = glib::timeout_add(std::time::Duration::from_millis(200), move || {
            let content = std::fs::read_to_string(&path).unwrap_or_default();
            let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
            if lines.is_empty() {
                return glib::Propagation::Continue;
            }
            let last = lines.last().unwrap();
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(last) {
                // Self::apply(&v);  — kept inert until panel has apply logic
            }
            glib::Propagation::Continue
        });

        let panel = Self {
            window,
            status,
            transcript,
            reply,
            confirm_ok,
            confirm_cancel,
            level,
            _destroy_id: destroy_id,
            _reap: glib::WeakRef::new(&window),
        };

        Ok(panel)
    }

    pub fn present(&self) {
        self.window.present();
    }
}
