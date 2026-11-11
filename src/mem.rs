//! Working-set readout against the 100 MB cap.
//! CADENCE_MEM=1 logs it every 5 s (like CADENCE_FPS); Settings shows `working_set_mb()`.

use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::Threading::GetCurrentProcess;

pub const CAP_MB: f64 = 100.0;

/// (working set, private bytes) in MiB.
pub fn usage_mb() -> (f64, f64) {
    let mut c = PROCESS_MEMORY_COUNTERS_EX { cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32, ..Default::default() };
    unsafe {
        let _ = GetProcessMemoryInfo(GetCurrentProcess(), (&mut c as *mut PROCESS_MEMORY_COUNTERS_EX).cast(), c.cb);
    }
    let mb = |b: usize| b as f64 / (1024.0 * 1024.0);
    (mb(c.WorkingSetSize), mb(c.PrivateUsage))
}

pub fn working_set_mb() -> f64 {
    usage_mb().0
}

/// Drops pages the process has not touched recently out of its working set (they stay
/// committed and come back on the next touch). Most of what it frees is the OpenGL driver's
/// one-time init memory, which is never touched again.
pub fn trim() {
    unsafe {
        let _ = windows::Win32::System::ProcessStatus::EmptyWorkingSet(GetCurrentProcess());
    }
}

/// Keeps the working set under the cap: trims at 6 s and 15 s after launch, then a
/// check every 30 s that trims above 85 MB. The GPU (OpenGL) driver allocates ~60 MB at init that
/// it never touches again; this hands those pages back to Windows. A check is one syscall.
pub fn start_guard() {
    let _ = std::thread::Builder::new().name("cadence-memguard".into()).spawn(|| {
        // Driver init is done a few seconds after the first frame; trim then and once more
        // after the first track and lyrics loaded.
        std::thread::sleep(std::time::Duration::from_secs(6));
        trim();
        std::thread::sleep(std::time::Duration::from_secs(9));
        trim();
        loop {
            std::thread::sleep(std::time::Duration::from_secs(30));
            let ws = working_set_mb();
            if ws > 85.0 {
                trim();
                if std::env::var("CADENCE_MEM").is_ok_and(|v| v == "1") {
                    crate::logln!("mem: trimmed at {ws:.1} MB");
                }
            }
        }
    });
}

/// Starts the CADENCE_MEM logger thread when the flag is set.
pub fn start_logger() {
    if !std::env::var("CADENCE_MEM").is_ok_and(|v| v == "1") {
        return;
    }
    let _ = std::thread::Builder::new().name("cadence-mem".into()).spawn(|| {
        let mut peak = 0.0f64;
        loop {
            let (ws, private) = usage_mb();
            peak = peak.max(ws);
            let flag = if ws > CAP_MB { " OVER CAP" } else { "" };
            crate::logln!("mem: working set {ws:.1} MB (peak {peak:.1}), private {private:.1} MB{flag}");
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
    });
}
