//! `cargo run --example popoutcheck`
//!
//! Opens the popped-out screen window with a picture in it, on its own, and reports what happened.
//!
//! It exists because a second operating-system window is the one part of the interface a unit test
//! cannot reach: `show_viewport_deferred` needs a real event loop, a real window server, and a callback
//! that egui may call from outside the frame that created it. The failure modes are all
//! runtime — a panic about being called from the wrong place, a closure that will not satisfy
//! `Send + Sync`, a texture that belongs to the wrong context — and every one of them would be found by
//! a person clicking the button rather than by `cargo test`.
//!
//! Runs for three seconds and exits with a verdict, so it can be run without anybody watching.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use boa_client::ui::popout::PopOut;

fn main() -> eframe::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let popout = PopOut::default();
    popout.open_for("A test pattern".to_string());

    // How many frames the popped-out window drew. The whole verdict rests on this being more than
    // one: a window that appears and never repaints is the failure that looks like success.
    let painted = Arc::new(AtomicU64::new(0));
    let verdict = painted.clone();

    let started = Instant::now();
    let mut texture: Option<egui::TextureHandle> = None;

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([420.0, 200.0]),
        ..Default::default()
    };

    // `run_ui_native` rather than `run_simple_native`: this eframe draws into a `Ui` rather than a
    // context, the same way the app itself does.
    eframe::run_ui_native("popoutcheck", options, move |ui, _frame| {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        // A moving test pattern, so a window that is drawing exactly one frame is distinguishable from
        // one that is drawing sixty.
        let phase = started.elapsed().as_secs_f32();
        let image = pattern(phase);
        match texture.as_mut() {
            Some(handle) => handle.set(image, egui::TextureOptions::LINEAR),
            None => texture = Some(ctx.load_texture("pattern", image, egui::TextureOptions::LINEAR)),
        }

        popout.update(texture.as_ref(), "A test pattern", Some("this is the status line".into()));
        let open = popout.show(ctx);
        if open {
            painted.fetch_add(1, Ordering::Relaxed);
            ctx.request_repaint_of(egui::ViewportId::from_hash_of("boa-screen-popout"));
        }

        ui.label("The picture should be in a window of its own.");
        ui.label(format!("popped out: {open}, frames: {}", painted.load(Ordering::Relaxed)));

        ctx.request_repaint();
        if started.elapsed() > Duration::from_secs(3) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    })?;

    let frames = verdict.load(Ordering::Relaxed);
    println!("the popped-out window was drawn {frames} times in three seconds");
    if frames > 30 {
        println!("OK: a second window opened and kept repainting");
    } else if frames > 0 {
        println!("SUSPECT: it opened but hardly repainted — a moving picture would stutter");
    } else {
        println!("FAILED: no second window was drawn at all");
    }
    Ok(())
}

/// A picture that changes with `phase`, so movement is visible.
fn pattern(phase: f32) -> egui::ColorImage {
    const W: usize = 320;
    const H: usize = 180;
    let mut pixels = Vec::with_capacity(W * H * 4);
    let offset = (phase * 60.0) as usize;
    for _row in 0..H {
        for x in 0..W {
            let bar = ((x + offset) / 20).is_multiple_of(2);
            let (r, g, b) = if bar { (52, 214, 92) } else { (14, 16, 22) };
            pixels.extend_from_slice(&[r, g, b, 255]);
        }
    }
    egui::ColorImage::from_rgba_unmultiplied([W, H], &pixels)
}
