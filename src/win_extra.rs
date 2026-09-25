//! Win 杂项：DWM 圆角、注册表开机启动之类

#[cfg(windows)]
pub mod win {
    use std::path::Path;
    use winreg::enums::*;
    use winreg::RegKey;

    const RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const APP_VALUE_NAME: &str = "YianLauncher";

 /// 任何窗口/托盘/热键 HWND 之前先调这个
 /// Manifest 已经写了 PerMonitorV2 的话这里会失败，无所谓 老包没嵌入 manifest 就靠它兜底
    pub fn ensure_per_monitor_dpi_v2() {
        use windows::Win32::UI::HiDpi::{
            SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE,
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        };
        unsafe {
            if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).is_err() {
                let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE);
            }
        }
    }

    pub fn set_auto_start(enabled: bool, exe: &Path) -> Result<(), String> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (key, _) = hkcu.create_subkey(RUN_SUBKEY).map_err(|e| e.to_string())?;
        if enabled {
            let s = exe.to_string_lossy().to_string();
            key.set_value(APP_VALUE_NAME, &s).map_err(|e| e.to_string())?;
        } else {
            let _ = key.delete_value(APP_VALUE_NAME);
        }
        Ok(())
    }

 /// 跟 `ui/app.slint` 里 palette-frame 对齐（COLORREF 是 0x00BBGGRR，别搞反）
    fn frame_border_colorref(skin_index: i32) -> u32 {
        match skin_index {
            1 => 0x00_fbf7f5, // #f5f7fb
            2 => 0x00_24303a, // #3a3024
            3 => 0x00_38242e, // #2e2438
            _ => 0x00_18100b, // #0b1018
        }
    }

    pub fn apply_rounded_corners(hwnd: isize, skin_index: i32) {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::Graphics::Dwm::{
            DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE,
            DWMWCP_ROUND,
        };
        let hwnd = HWND(hwnd as *mut std::ffi::c_void);
        unsafe {
            use windows::Win32::Graphics::Gdi::{SetWindowRgn, HRGN};
 // 旧版用过 SetWindowRgn，不清掉的话系统浅色直角边框会一直露着
            let _ = SetWindowRgn(hwnd, HRGN(std::ptr::null_mut()), true);
            let mut corner = DWMWCP_ROUND.0 as u32;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &mut corner as *mut u32 as *const _,
                std::mem::size_of::<u32>() as u32,
            );
            let mut border = frame_border_colorref(skin_index);
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_BORDER_COLOR,
                &mut border as *mut u32 as *const _,
                std::mem::size_of::<u32>() as u32,
            );
        }
    }

 /// 光标屏幕坐标，物理像素（跟 winit 那套一样）
    pub fn cursor_screen_physical_position() -> (i32, i32) {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
        unsafe {
            let mut p = POINT::default();
            let _ = GetCursorPos(&mut p);
            (p.x, p.y)
        }
    }

 /// 点 (x,y) 落在哪块屏上，返回那块屏的工作区（不含任务栏），物理像素
    pub fn monitor_work_area_at_point(x: i32, y: i32) -> (i32, i32, i32, i32) {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::Graphics::Gdi::{
            GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
        };
        use windows::Win32::UI::WindowsAndMessaging::{
            GetSystemMetrics, SM_CYVIRTUALSCREEN, SM_CXVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
        };

        unsafe {
            let pt = POINT { x, y };
            let hmon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if GetMonitorInfoW(hmon, &mut mi).as_bool() {
                let r = mi.rcWork;
                return (r.left, r.top, r.right, r.bottom);
            }

            let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
            let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
            let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
            (vx, vy, vx.saturating_add(vw), vy.saturating_add(vh))
        }
    }

 /// 把窗口左上角夹进当前屏工作区，托盘弹窗别飞出屏幕外
 /// x,y 是想放的位置 搞不定就退回虚拟屏矩形
    pub fn clamp_window_top_left_for_size(x: i32, y: i32, width_px: i32, height_px: i32) -> (i32, i32) {
        let clamp_to_rect = |left: i32, top: i32, right: i32, bottom: i32| -> (i32, i32) {
            let ww = (right - left).max(1);
            let wh = (bottom - top).max(1);
            let nx = if width_px >= ww {
                left
            } else {
                x.max(left).min(right - width_px)
            };
            let ny = if height_px >= wh {
                top
            } else {
                y.max(top).min(bottom - height_px)
            };
            (nx, ny)
        };

        let (left, top, right, bottom) = monitor_work_area_at_point(x, y);
        clamp_to_rect(left, top, right, bottom)
    }

 /// 喊一声 EmptyWorkingSet，任务管理器里数字好看点，业务状态不受影响
    pub fn trim_current_process_working_set() {
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::ProcessStatus::K32EmptyWorkingSet;
        use windows::Win32::System::Threading::GetCurrentProcess;
        unsafe {
            let h: HANDLE = GetCurrentProcess();
            let _ = K32EmptyWorkingSet(h);
        }
    }

 /// 无框窗下 Window::is_maximized() 经常撒谎，直接问系统 IsZoomed 靠谱
    pub fn is_zoomed_window(hwnd: isize) -> bool {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::IsZoomed;
        let hwnd = HWND(hwnd as *mut std::ffi::c_void);
        unsafe { IsZoomed(hwnd).as_bool() }
    }

 /// winit 靠 WM_EXITSIZEMOVE 清掉 drag 用的内部 dragging 标志
 ///
 /// 这里必须 SendMessageW（同步）！Slint 和 winit 同线程，drag_window 会立刻执行
 /// 要是 PostMessageW，消息还在队列里、dragging 还是 true，标题栏就再也拖不动了
 /// 踩过坑，别改回去
    pub fn post_wm_exit_sizemove(hwnd: isize) {
        use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::{SendMessageW, WM_EXITSIZEMOVE};
        let hwnd = HWND(hwnd as *mut std::ffi::c_void);
        unsafe {
            let _ = SendMessageW(hwnd, WM_EXITSIZEMOVE, WPARAM(0), LPARAM(0));
        }
    }

 /// 主窗整体透明度 50～100 拉到 100 就把 layered 样式卸了
    pub fn apply_window_opacity(hwnd: isize, percent: u8) {
        use windows::Win32::Foundation::{COLORREF, HWND};
        use windows::Win32::UI::WindowsAndMessaging::{
            GetWindowLongPtrW, SetLayeredWindowAttributes, SetWindowLongPtrW, SetWindowPos,
            GWL_EXSTYLE, LWA_ALPHA, SWP_FRAMECHANGED, SWP_NOMOVE, SWP_NOACTIVATE, SWP_NOSIZE,
            SWP_NOZORDER, WS_EX_LAYERED,
        };
        let hwnd = HWND(hwnd as *mut std::ffi::c_void);
        let pct = percent.clamp(50, 100);
        unsafe {
            let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
            if pct >= 100 {
                if style & WS_EX_LAYERED.0 != 0 {
                    let _ = SetWindowLongPtrW(
                        hwnd,
                        GWL_EXSTYLE,
                        (style & !WS_EX_LAYERED.0) as isize,
                    );
                }
            } else {
                if style & WS_EX_LAYERED.0 == 0 {
                    let _ = SetWindowLongPtrW(
                        hwnd,
                        GWL_EXSTYLE,
                        (style | WS_EX_LAYERED.0) as isize,
                    );
                }
                let alpha = ((pct as u32 * 255) / 100).min(255) as u8;
                let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA);
            }
 // DWM 圆角 / winit 改 Z 序之后，layered alpha 偶尔被系统清掉，刷一帧
            let _ = SetWindowPos(
                hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        }
    }
}

#[cfg(not(windows))]
pub mod win {
    use std::path::Path;
    pub fn ensure_per_monitor_dpi_v2() {}
    pub fn set_auto_start(_enabled: bool, _exe: &Path) -> Result<(), String> {
        Ok(())
    }
    pub fn apply_rounded_corners(_hwnd: isize, _skin_index: i32) {}

    pub fn cursor_screen_physical_position() -> (i32, i32) {
        (0, 0)
    }

    pub fn monitor_work_area_at_point(_x: i32, _y: i32) -> (i32, i32, i32, i32) {
        (0, 0, 1920, 1080)
    }

    pub fn clamp_window_top_left_for_size(x: i32, y: i32, _width_px: i32, _height_px: i32) -> (i32, i32) {
        (x, y)
    }

    pub fn trim_current_process_working_set() {}

    pub fn is_zoomed_window(_hwnd: isize) -> bool {
        false
    }

    pub fn post_wm_exit_sizemove(_hwnd: isize) {}

    pub fn apply_window_opacity(_hwnd: isize, _percent: u8) {}
}
