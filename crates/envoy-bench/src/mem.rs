//! Another process's memory, read from outside, the way each OS's own monitor counts it:
//!
//! - macOS: physical footprint (`proc_pid_rusage`), what Activity Monitor's Memory column shows.
//! - Linux: resident set size (`VmRSS` in `/proc/PID/status`).
//! - Windows: working set (`GetProcessMemoryInfo`), which includes shared pages: a little
//!   above Task Manager's private working set, so the stricter of the two.

/// Bytes, or `None` when it can't be read (process gone, unsupported OS).
pub fn of(pid: u32) -> Option<u64> {
    imp(pid)
}

pub const METRIC: &str = if cfg!(target_os = "macos") {
    "physical footprint"
} else if cfg!(windows) {
    "working set"
} else {
    "RSS"
};

#[cfg(target_os = "linux")]
fn imp(pid: u32) -> Option<u64> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let line = s.lines().find(|l| l.starts_with("VmRSS:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

#[cfg(target_os = "macos")]
fn imp(pid: u32) -> Option<u64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage_info_v4>::zeroed();
    // SAFETY: a zeroed buffer of the size RUSAGE_INFO_V4 fills; read only on success.
    unsafe {
        if libc::proc_pid_rusage(pid as i32, libc::RUSAGE_INFO_V4, usage.as_mut_ptr().cast()) != 0 {
            return None;
        }
        Some(usage.assume_init().ri_phys_footprint)
    }
}

#[cfg(windows)]
fn imp(pid: u32) -> Option<u64> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ};
    // SAFETY: plain Win32 calls on a handle we open and close here, into a sized struct.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, 0, pid);
        if h.is_null() {
            return None;
        }
        let mut c: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        let ok = GetProcessMemoryInfo(h, &mut c, c.cb) != 0;
        CloseHandle(h);
        ok.then_some(c.WorkingSetSize as u64)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn imp(_pid: u32) -> Option<u64> {
    None
}
