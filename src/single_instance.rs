//! 单实例：Windows 命名 Mutex，别开两个启动器抢配置

#[cfg(windows)]
pub struct SingleInstanceMutex(windows::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for SingleInstanceMutex {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

/// 抢到锁就当老大 已经有人在跑就返回 None
#[cfg(windows)]
pub fn try_first_instance() -> anyhow::Result<Option<SingleInstanceMutex>> {
    use anyhow::Context;
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, FALSE, HANDLE};
    use windows::Win32::Foundation::GetLastError;
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::core::PCWSTR;

    let mut wide: Vec<u16> = OsStr::new(r"Local\YianLauncherSingleInstance")
        .encode_wide()
        .chain(Some(0))
        .collect();

    unsafe {
        let h: HANDLE = CreateMutexW(None, FALSE, PCWSTR(wide.as_mut_ptr()))
            .context("创建单实例互斥量失败")?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = windows::Win32::Foundation::CloseHandle(h);
            return Ok(None);
        }
        Ok(Some(SingleInstanceMutex(h)))
    }
}

#[cfg(not(windows))]
pub struct SingleInstanceMutex;

#[cfg(not(windows))]
pub fn try_first_instance() -> anyhow::Result<Option<SingleInstanceMutex>> {
    Ok(Some(SingleInstanceMutex))
}
