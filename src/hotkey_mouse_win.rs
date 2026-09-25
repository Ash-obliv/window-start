//! Win 上用 WH_MOUSE_LL 听全局鼠标键 只认中键和两个侧键

use crate::custom_hotkey::MouseButton;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, SetWindowsHookExW, UnhookWindowsHookEx, HC_ACTION, HHOOK, MSLLHOOKSTRUCT,
    WH_MOUSE_LL, WM_MBUTTONDOWN, WM_XBUTTONDOWN,
};

// 钩子句柄塞 Atomic 里，省得搞复杂生命周期
static HOOK: AtomicUsize = AtomicUsize::new(0);
static BINDING: Mutex<Option<MouseButton>> = Mutex::new(None);
static SUPPRESS: AtomicBool = AtomicBool::new(false);
static TRIGGER: Mutex<Option<crate::custom_hotkey::HotkeyTrigger>> = Mutex::new(None);

fn hook_handle() -> Option<HHOOK> {
    let raw = HOOK.load(Ordering::Relaxed);
    if raw == 0 {
        None
    } else {
        Some(HHOOK(raw as *mut core::ffi::c_void))
    }
}

pub fn set_trigger(cb: crate::custom_hotkey::HotkeyTrigger) {
    *TRIGGER.lock().unwrap() = Some(cb);
}

/// 录入快捷键那段时间 true，钩子直接放行不回调
pub fn set_suppress(suppress: bool) {
    SUPPRESS.store(suppress, Ordering::Relaxed);
}

pub fn install(btn: MouseButton) -> anyhow::Result<()> {
    uninstall();
    *BINDING.lock().unwrap() = Some(btn);
    unsafe {
        let hook = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), None, 0)?;
        HOOK.store(hook.0 as usize, Ordering::Relaxed);
    }
    Ok(())
}

pub fn uninstall() {
    *BINDING.lock().unwrap() = None;
    if let Some(hook) = hook_handle() {
        HOOK.store(0, Ordering::Relaxed);
        unsafe {
            let _ = UnhookWindowsHookEx(hook);
        }
    }
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
 // 对上绑定的键就喊一声触发 别吞消息，继续往下传
    if code == HC_ACTION as i32 && !SUPPRESS.load(Ordering::Relaxed) {
        if let Some(expected) = *BINDING.lock().unwrap() {
            if let Some(pressed) = msg_to_button(wparam, lparam) {
                if pressed == expected {
                    if let Some(cb) = TRIGGER.lock().unwrap().as_ref() {
                        cb();
                    }
                }
            }
        }
    }
    unsafe {
        if let Some(hook) = hook_handle() {
            CallNextHookEx(hook, code, wparam, lparam)
        } else {
            LRESULT(0)
        }
    }
}

fn msg_to_button(wparam: WPARAM, lparam: LPARAM) -> Option<MouseButton> {
    let msg = wparam.0 as u32;
    match msg {
        WM_MBUTTONDOWN => Some(MouseButton::Middle),
        WM_XBUTTONDOWN => {
 // 侧键编号藏在 mouseData 高位，Win 文档那套
            let hook = unsafe { *(lparam.0 as *const MSLLHOOKSTRUCT) };
            let xbtn = (hook.mouseData >> 16) as u16;
            if xbtn == 1 {
                Some(MouseButton::X1)
            } else if xbtn == 2 {
                Some(MouseButton::X2)
            } else {
                None
            }
        }
        _ => None,
    }
}
