use gtk4::prelude::*;

mod panel;
use panel::Panel;

fn main() {
    let app = gtk4::Application::builder()
        .application_id("com.arc-panel")
        .flags(gtk4::ApplicationFlags::empty())
        .build();
    app.connect_activate(|app| {
        let panel = Panel::new(app).unwrap();
        panel.present();
    });
    app.run();
}
