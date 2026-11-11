//! Plugin scanning on a background thread.

use carlita_core::discovery::{scan_all, ScanProgress, ScanReport};
use carlita_core::PluginFormat;
use crossbeam_channel::{unbounded, Receiver};
use std::path::PathBuf;
use std::time::Duration;

pub enum ScanMsg {
    Progress(ScanProgress),
    Done(ScanReport),
}

pub fn start(discovery_tool: PathBuf) -> Receiver<ScanMsg> {
    let (tx, rx) = unbounded();
    std::thread::Builder::new()
        .name("carlita-scan".into())
        .spawn(move || {
            let progress_tx = tx.clone();
            let report = scan_all(
                &discovery_tool,
                &PluginFormat::ALL,
                Duration::from_secs(20),
                move |p| {
                    let _ = progress_tx.send(ScanMsg::Progress(p));
                },
            );
            let _ = tx.send(ScanMsg::Done(report));
        })
        .expect("spawn scan thread");
    rx
}
