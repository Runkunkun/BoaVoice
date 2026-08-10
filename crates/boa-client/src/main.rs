//! BoaVoice — entry point and window setup.
//!
//! Most of what happens here is negotiating the window that the glass look requires. Three settings
//! have to line up, and the effect collapses if any one is missing:
//!
//! * `with_transparent(true)` so the GPU surface composites rather than covering what is behind it,
//! * `with_fullsize_content_view` plus a hidden title bar, so our own chrome runs edge to edge and
//!   macOS still draws its window controls over it, and
//! * the platform backdrop that [`boa_client::platform`] installs on the first frame, which is what
//!   actually blurs the desktop behind the window.
//!
//! On Linux and Windows the first two still apply — the window is translucent and the app draws its
//! own chrome — and the third is a no-op, which looks deliberate rather than broken.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use boa_client::ui::App;

/// The app's icon, for the dock, the task bar and the window.
///
/// Built by `scripts/make-icon.py` from `packaging/boa-source.svg` and committed, so a plain
/// checkout builds without librsvg or a Python interpreter.
const ICON: &[u8] = include_bytes!("../../../packaging/icon-512.png");

fn main() -> eframe::Result<()> {
    // `RUST_LOG=boa_client=debug` for the network and audio detail, which is logged rather than
    // surfaced.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Before anything else: launched from Finder, a `.desktop` entry or a Start-menu shortcut there is
    // nowhere for a panic to go, so it gets written to a file instead. See `diagnostics`.
    boa_client::diagnostics::install();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("BoaVoice")
            .with_app_id("boavoice")
            .with_inner_size([1180.0, 760.0])
            // Below this the voice bar's controls start overlapping the name beside them.
            .with_min_inner_size([880.0, 560.0])
            .with_transparent(true)
            .with_fullsize_content_view(true)
            .with_titlebar_shown(false)
            .with_title_shown(false)
            // The window's own shadow is kept: it separates the frosted panel from the desktop,
            // which the blur alone does not do.
            .with_has_shadow(true)
            .with_icon(load_icon()),
        // wgpu rather than glow: the glass look leans on large translucent fills that get
        // recomposited every frame, and the GPU path keeps that at display rate — which matters more
        // here than in the music players, because a screen share is a full-window texture upload on
        // top of it.
        renderer: eframe::Renderer::Wgpu,
        // Vector glyphs and rounded panels are drawn from paths, and egui anti-aliases those itself;
        // MSAA on top would cost fill rate for no visible gain.
        multisampling: 0,
        ..Default::default()
    };

    let result = eframe::run_native("BoaVoice", options, Box::new(|cc| Ok(Box::new(App::new(cc)))));

    match &result {
        Ok(()) => boa_client::diagnostics::record_clean_exit("event loop finished"),
        Err(err) => boa_client::diagnostics::record_clean_exit(&format!("event loop failed: {err}")),
    }
    result
}

/// Decode the bundled PNG for the window icon.
///
/// A failure here is cosmetic — the app runs fine with the system's default icon — so it degrades
/// to a blank rather than aborting startup.
fn load_icon() -> egui::IconData {
    match image::load_from_memory(ICON) {
        Ok(image) => {
            let rgba = image.to_rgba8();
            egui::IconData { width: rgba.width(), height: rgba.height(), rgba: rgba.into_raw() }
        }
        Err(err) => {
            log::warn!("icon: {err}");
            egui::IconData::default()
        }
    }
}
