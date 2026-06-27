#[allow(dead_code)]
pub(crate) mod app;

#[allow(dead_code)]
pub(crate) mod confirm;

#[allow(dead_code)]
pub(crate) mod input;

#[allow(dead_code)]
pub(crate) mod ui;

use crate::conn::Connection;

/// Phase 1.5b TUI 入口。Task 4+ 实装；Task 3 仅为占位以打通 dispatch。
pub(crate) async fn run_tui(conn: Connection) -> Result<(), Box<dyn std::error::Error>> {
    let _ = conn;
    eprintln!("TUI not yet implemented (Phase 1.5b — Task 3 桩)");
    Ok(())
}
