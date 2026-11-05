//! Frame-rate conversion and cancellation with exact hardware encoder counter assertions.
//! Kept in its own test binary: unrelated platform tests must not change these process-wide
//! counters while the cancellation and follow-up export run (docs/testing.md).
#![cfg(target_os = "macos")]

#[path = "it/common/hardware_encode.rs"]
mod support;

use filmcraft_export::{ExportSettings, Progress, export, hw_encode_stats};
use filmcraft_time::FrameRate;
use support::{H, W, hardware, hardware_settings, inspect, project, tmp};

/// An export at another frame rate than the sequence's (frames are repeated or dropped on the way) and
/// a cancelled export followed by a complete one in the same process: the hardware path must give the
/// asked-for frame count, release its sessions and start clean again.
#[test]
fn rate_conversion_and_cancel_then_export_again() {
    filmcraft_platform::register();
    let probe = ExportSettings { include_audio: false, ..hardware_settings() };
    if hardware(&probe, W, H).is_none() {
        return;
    }
    let (frames, (p, seq, m)) = (60i64, project(60));
    // 60 frames at 30 fps become 120 at 60 fps and 50 at 25 fps
    for (rate, want) in [(FrameRate::FPS_60, 120usize), (FrameRate::FPS_25, 50)] {
        let path = tmp(&format!("rate-{}.mp4", rate.num));
        let s = ExportSettings { path: path.clone(), frame_rate: Some(rate), ..probe.clone() };
        let report = export(&p, seq, &s, &m, &Progress::default()).expect("export");
        assert_eq!(report.frames as usize, want, "{rate:?}");
        let (samples, pictures, _) = inspect(&path);
        assert_eq!((samples, pictures), (want, want), "{rate:?}");
    }
    // cancel from another thread once a few frames are done
    let before = hw_encode_stats();
    let path = tmp("cancelled.mp4");
    let s = ExportSettings { path: path.clone(), ..probe.clone() };
    let progress = Progress::default();
    let result = std::thread::scope(|scope| {
        scope.spawn(|| {
            while progress.done.load(std::sync::atomic::Ordering::Relaxed) < 8 && !progress.finished.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            progress.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        });
        export(&p, seq, &s, &m, &progress)
    });
    match result {
        Err(filmcraft_export::ExportError::Cancelled) => {}
        Ok(r) => eprintln!("the export finished before the cancel landed ({} frames)", r.frames),
        Err(e) => panic!("a cancelled export must report Cancelled or finish, not {e}"),
    }
    // and the next export in the same process is complete
    let path = tmp("after-cancel.mp4");
    let s = ExportSettings { path: path.clone(), ..probe };
    let report = export(&p, seq, &s, &m, &Progress::default()).expect("export after a cancel");
    assert_eq!(report.frames, frames as u64);
    let (samples, pictures, _) = inspect(&path);
    assert_eq!((samples, pictures), (frames as usize, frames as usize));
    let after = hw_encode_stats();
    assert!(after.sessions >= before.sessions + 2, "both exports opened a hardware session");
    assert_eq!(after.declined, before.declined, "nothing was declined to the software encoder");
}
