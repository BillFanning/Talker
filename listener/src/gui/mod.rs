//! GUI presentation layer (spec §3, listener ADR-008).
//!
//! **Scaffold.** This launches the eframe window and proves the `--gui` dispatch
//! and toolchain. The runtime bridge — a background Tokio "driver" task that owns
//! the [`Listener`](crate::runtime::Listener) and exchanges `UiCommand`/`UiUpdate`
//! over channels, calling `egui::Context::request_repaint` on each update — and the
//! real panes land in the next steps (ADR-008 build order). Per AGENTS §5 the egui
//! code here stays a thin presentation layer: it never owns the runtime, never does
//! I/O, and never blocks.

use anyhow::anyhow;

/// Launch the graphical interface (§3). Owns the eframe event loop on the calling
/// (main) thread; the runtime bridge will run on its own Tokio thread (ADR-008).
pub fn run() -> anyhow::Result<()> {
    crate::diagnostics::init_logging(); // §114; non-fatal if already installed (§117)
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1100.0, 740.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Listener",
        options,
        Box::new(|_cc| Ok(Box::new(ListenerApp::default()))),
    )
    .map_err(|e| anyhow!("{e}"))
}

/// The eframe application root. A scaffold today; it will hold the `UiCommand`
/// sender, the `UiUpdate` receiver, and the folded per-Channel view-model the panes
/// render (ADR-008).
#[derive(Default)]
struct ListenerApp {}

impl eframe::App for ListenerApp {
    // This workspace's eframe surfaces a `Ui` directly (App::ui), like talker's GUI,
    // rather than the upstream `update(ctx, …)`; draw into the provided `ui`.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.heading("Listener");
        ui.label("Graphical interface — under construction.");
        ui.separator();
        ui.label(
            "Next: a background driver task owns the runtime and exchanges \
             commands and channel snapshots with this window over channels \
             (ADR-008). For now, use the headless CLI: `listener --udp <port>`.",
        );
    }
}
