// Print what the daemon's own resolver makes of the real module's output.
use arc_config::Music;
use arc_daemon::music::{Resolver, YtMusicApi};

fn main() {
    let cfg = Music {
        python: "/home/ciancom/.local/share/arc/venv/bin/python".into(),
        resolve_timeout_s: 30,
        ..Music::default()
    };
    let r = YtMusicApi::new(&cfg);
    match r.resolve("boards of canada roygbiv", 1) {
        Ok(tracks) => {
            for t in tracks {
                println!(
                    "title={:?} artist={:?} artwork_len={} artwork={:?}",
                    t.title,
                    t.artist,
                    t.artwork.len(),
                    if t.artwork.len() > 60 { &t.artwork[..60] } else { &t.artwork }
                );
            }
        }
        Err(e) => println!("ERROR: {e}"),
    }
}