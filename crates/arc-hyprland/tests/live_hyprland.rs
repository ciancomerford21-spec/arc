//! Live tests against the running Hyprland. They only ever act on a
//! throwaway `arc-probe` window parked on a hidden special workspace, and
//! never change the user's active workspace or focus.
//!
//! Skipped automatically when Hyprland isn't running. Run with:
//! `cargo test -p arc-hyprland --test live_hyprland -- --test-threads=1`

use arc_hyprland::state::DesktopTracker;
use arc_hyprland::{Dispatch, FloatAction, Hyprland, WindowSel};
use std::time::Duration;

fn hypr() -> Option<Hyprland> {
    match Hyprland::connect() {
        Ok(h) => Some(h),
        Err(e) => {
            eprintln!("skipping live Hyprland test: {e}");
            None
        }
    }
}

async fn wait_for<F: Fn(&[arc_hyprland::Client]) -> bool>(h: &Hyprland, f: F) -> Vec<arc_hyprland::Client> {
    for _ in 0..50 {
        let c = h.clients().await.unwrap();
        if f(&c) {
            return c;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("condition not met within 5s");
}

#[tokio::test]
async fn queries_work() {
    let Some(h) = hypr() else { return };
    let v = h.version().await.unwrap();
    assert!(v.get("version").is_some() || v.get("tag").is_some(), "{v}");
    assert!(!h.monitors().await.unwrap().is_empty());
    let ws = h.active_workspace().await.unwrap();
    assert!(ws.id != 0);
    h.workspaces().await.unwrap();
    h.active_window().await.unwrap();
}

#[tokio::test]
async fn probe_window_lifecycle() {
    let Some(h) = hypr() else { return };
    let before_ws = h.active_workspace().await.unwrap().id;
    let tag = format!("arcprobe{}", std::process::id());
    let class = "arc-probe";
    h.dispatch(&Dispatch::Exec {
        cmd: format!("kitty --class {class} --title {tag} -e sleep 30"),
        workspace: Some(format!("special:{tag} silent")),
    })
    .await
    .unwrap();
    let clients = wait_for(&h, |c| {
        c.iter().any(|w| w.title == tag || (w.class == class && w.workspace.name == format!("special:{tag}")))
    })
    .await;
    let w = clients.iter().find(|w| w.workspace.name == format!("special:{tag}")).unwrap().clone();
    let sel = WindowSel::Address(w.address.clone());

    h.dispatch(&Dispatch::Float { window: sel.clone(), action: FloatAction::Enable }).await.unwrap();
    h.dispatch(&Dispatch::Resize { window: sel.clone(), x: 640, y: 400, relative: false }).await.unwrap();
    h.dispatch(&Dispatch::MoveTo { window: sel.clone(), x: 100, y: 120 }).await.unwrap();
    let c = wait_for(&h, |c| c.iter().any(|x| x.address == w.address && x.floating && x.size == [640, 400]))
        .await;
    let got = c.iter().find(|x| x.address == w.address).unwrap();
    assert_eq!(got.at, [100, 120]);

    let other = format!("special:{tag}b");
    h.dispatch(&Dispatch::MoveToWorkspace { window: sel.clone(), workspace: other.clone(), follow: false })
        .await
        .unwrap();
    wait_for(&h, |c| c.iter().any(|x| x.address == w.address && x.workspace.name == other)).await;

    // Tracker sees the window and resolves it by name.
    let t = DesktopTracker::new(Some(h.clone()));
    t.refresh().await.unwrap();
    let s = t.snapshot().await;
    assert!(s.connected && s.client(&w.address).is_some());

    h.dispatch(&Dispatch::Close(sel)).await.unwrap();
    wait_for(&h, |c| !c.iter().any(|x| x.address == w.address)).await;

    assert_eq!(h.active_workspace().await.unwrap().id, before_ws, "test must not switch workspaces");
}

#[tokio::test]
async fn bad_dispatch_is_reported() {
    let Some(h) = hypr() else { return };
    let r = h.request("dispatch hl.dsp.definitely_not_real()").await.unwrap();
    assert_ne!(r.trim(), "ok");
}
