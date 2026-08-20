use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskBudget {
    Probe,
    Fixed(u64),
}

impl DiskBudget {
    pub fn available(&self, path: &Path) -> Option<u64> {
        match self {
            DiskBudget::Fixed(bytes) => Some(*bytes),
            DiskBudget::Probe => probe_available(path),
        }
    }
}

pub fn admits(estimate: u64, available: u64, reserved: u64) -> bool {
    estimate.saturating_add(reserved) <= available
}

#[allow(clippy::unnecessary_cast)]
fn probe_available(path: &Path) -> Option<u64> {
    let raw = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(raw.as_ptr(), &mut stats) } != 0 {
        return None;
    }
    Some((stats.f_bavail as u64).saturating_mul(stats.f_frsize as u64))
}
