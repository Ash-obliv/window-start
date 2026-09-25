//! 全局快捷键这一坨：键盘走 global-hotkey，鼠标走 Win 低级钩子

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::GlobalHotKeyManager;
use std::str::FromStr;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HotkeyBinding {
    Keyboard(HotKey),
    Mouse(MouseButton),
}

/// 左右键坚决不绑 误触率太高 中键 + 两个侧键够用了
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Middle,
    X1,
    X2,
}

const KB_PREFIX: &str = "kb:";
const MOUSE_PREFIX: &str = "mouse:";

/// 老配置还是 `preset:0` 那种，读进来时顺手换成现在能存的字符串
pub fn migrate_legacy_hotkey(hotkey: &mut Option<String>) {
    let Some(s) = hotkey.as_ref() else {
        return;
    };
    if let Some(n) = s.strip_prefix("preset:").and_then(|x| x.parse::<i32>().ok()) {
        *hotkey = Some(match n {
            0 => format!("{KB_PREFIX}alt+Backquote"),
            1 => format!("{KB_PREFIX}alt+Space"),
            2 => format!("{KB_PREFIX}alt+Digit1"),
            _ => format!("{KB_PREFIX}alt+Backquote"),
        });
    }
}

/// 从 config 字符串还原绑定 认不出来就当没设
pub fn binding_from_storage(raw: &Option<String>) -> Option<HotkeyBinding> {
    let s = raw.as_ref()?;
    if let Some(rest) = s.strip_prefix(KB_PREFIX) {
        let hk = HotKey::from_str(rest).ok()?;
        return Some(HotkeyBinding::Keyboard(hk));
    }
    if let Some(rest) = s.strip_prefix(MOUSE_PREFIX) {
        let btn = match rest {
            "middle" => MouseButton::Middle,
            "x1" => MouseButton::X1,
            "x2" => MouseButton::X2,
            _ => return None,
        };
        return Some(HotkeyBinding::Mouse(btn));
    }
    None
}

/// 存盘用，前面带 `kb:`  / `mouse:` 前缀好分辨
pub fn binding_to_storage(b: &HotkeyBinding) -> String {
    match b {
        HotkeyBinding::Keyboard(hk) => format!("{KB_PREFIX}{}", hk.into_string()),
        HotkeyBinding::Mouse(btn) => format!(
            "{MOUSE_PREFIX}{}",
            match btn {
                MouseButton::Middle => "middle",
                MouseButton::X1 => "x1",
                MouseButton::X2 => "x2",
            }
        ),
    }
}

/// UI 上给人看的文案，比如 `Alt + ~`、`鼠标中键`
pub fn binding_label(b: &HotkeyBinding) -> String {
    match b {
        HotkeyBinding::Keyboard(hk) => keyboard_label(hk),
        HotkeyBinding::Mouse(btn) => mouse_label(*btn),
    }
}

pub fn default_capture_hint() -> &'static str {
    "点击此处，按下键盘组合键，或鼠标中键/侧键"
}

fn keyboard_label(hk: &HotKey) -> String {
    let mut parts = Vec::new();
    if hk.mods.contains(Modifiers::CONTROL) {
        parts.push("Ctrl");
    }
    if hk.mods.contains(Modifiers::ALT) {
        parts.push("Alt");
    }
    if hk.mods.contains(Modifiers::SHIFT) {
        parts.push("Shift");
    }
    if hk.mods.contains(Modifiers::SUPER) {
        parts.push("Win");
    }
    parts.push(code_display_name(hk.key));
    parts.join(" + ")
}

/// Code 枚举名太难看，换成正常人能认的键名
fn code_display_name(code: Code) -> &'static str {
    use Code::*;
    match code {
        Backquote => "~",
        Space => "Space",
        Digit0 => "0",
        Digit1 => "1",
        Digit2 => "2",
        Digit3 => "3",
        Digit4 => "4",
        Digit5 => "5",
        Digit6 => "6",
        Digit7 => "7",
        Digit8 => "8",
        Digit9 => "9",
        KeyA => "A",
        KeyB => "B",
        KeyC => "C",
        KeyD => "D",
        KeyE => "E",
        KeyF => "F",
        KeyG => "G",
        KeyH => "H",
        KeyI => "I",
        KeyJ => "J",
        KeyK => "K",
        KeyL => "L",
        KeyM => "M",
        KeyN => "N",
        KeyO => "O",
        KeyP => "P",
        KeyQ => "Q",
        KeyR => "R",
        KeyS => "S",
        KeyT => "T",
        KeyU => "U",
        KeyV => "V",
        KeyW => "W",
        KeyX => "X",
        KeyY => "Y",
        KeyZ => "Z",
        F1 => "F1",
        F2 => "F2",
        F3 => "F3",
        F4 => "F4",
        F5 => "F5",
        F6 => "F6",
        F7 => "F7",
        F8 => "F8",
        F9 => "F9",
        F10 => "F10",
        F11 => "F11",
        F12 => "F12",
        Tab => "Tab",
        Enter => "Enter",
        Escape => "Esc",
        Backspace => "Backspace",
        Delete => "Delete",
        Insert => "Insert",
        Home => "Home",
        End => "End",
        PageUp => "PageUp",
        PageDown => "PageDown",
        ArrowUp => "↑",
        ArrowDown => "↓",
        ArrowLeft => "←",
        ArrowRight => "→",
        _ => "键", // 实在不认识就糊一个
    }
}

fn mouse_label(btn: MouseButton) -> String {
    match btn {
        MouseButton::Middle => "鼠标中键".into(),
        MouseButton::X1 => "鼠标侧键 1".into(),
        MouseButton::X2 => "鼠标侧键 2".into(),
    }
}

/// Slint 那边传来的数字：0 中键，1 侧键 back，2 侧键 forward
pub fn slint_mouse_button(btn: i32) -> Option<MouseButton> {
    Some(match btn {
        0 => MouseButton::Middle,
        1 => MouseButton::X1,
        2 => MouseButton::X2,
        _ => return None,
    })
}

/// 卸掉当前生效的热键（键盘槽 + 鼠标钩子都清）
pub fn unregister_active(
    mgr: &mut GlobalHotKeyManager,
    kb_slot: &mut Option<HotKey>,
) {
    if let Some(old) = kb_slot.take() {
        let _ = mgr.unregister(old);
    }
    #[cfg(windows)]
    crate::hotkey_mouse_win::uninstall();
}

/// 按配置重新挂上热键 先卸再装免得残留
pub fn register_active(
    mgr: &mut GlobalHotKeyManager,
    kb_slot: &mut Option<HotKey>,
    enabled: bool,
    binding: Option<&HotkeyBinding>,
) -> Result<(), String> {
    unregister_active(mgr, kb_slot);
    if !enabled {
        return Ok(());
    }
    let Some(binding) = binding else {
        return Ok(());
    };
    match binding {
        HotkeyBinding::Keyboard(hk) => {
            mgr.register(*hk).map_err(|e| format!("{}", e))?;
            *kb_slot = Some(*hk);
            Ok(())
        }
        HotkeyBinding::Mouse(btn) => {
            #[cfg(windows)]
            {
                crate::hotkey_mouse_win::install(*btn).map_err(|e| e.to_string())
            }
            #[cfg(not(windows))]
            {
                let _ = btn;
                Err(String::from("鼠标全局快捷键仅支持 Windows"))
            }
        }
    }
}

/// 正在录入快捷键时把鼠标钩子静音一下，不然一按就触发切换，录不了
pub fn set_capture_suppress_mouse(suppress: bool) {
    #[cfg(windows)]
    crate::hotkey_mouse_win::set_suppress(suppress);
    #[cfg(not(windows))]
    let _ = suppress;
}

/// winit 修饰键 → global-hotkey 那套 flags
pub fn winit_modifiers(m: winit::keyboard::ModifiersState) -> Modifiers {
    let mut out = Modifiers::empty();
    if m.shift_key() {
        out |= Modifiers::SHIFT;
    }
    if m.control_key() {
        out |= Modifiers::CONTROL;
    }
    if m.alt_key() {
        out |= Modifiers::ALT;
    }
    if m.super_key() {
        out |= Modifiers::SUPER;
    }
    out
}

/// 光按修饰键不算完整快捷键，录入时要跳过
pub fn is_modifier_code(code: Code) -> bool {
    matches!(
        code,
        Code::ShiftLeft
            | Code::ShiftRight
            | Code::ControlLeft
            | Code::ControlRight
            | Code::AltLeft
            | Code::AltRight
            | Code::MetaLeft
            | Code::MetaRight
    )
}

/// winit 物理键 → HotKey 的 Code 一大坨 match 没啥好看的
pub fn winit_physical_key_to_code(key: winit::keyboard::PhysicalKey) -> Option<Code> {
    use winit::keyboard::{KeyCode, PhysicalKey};
    let PhysicalKey::Code(key) = key else {
        return None;
    };
    Some(match key {
        KeyCode::Backquote => Code::Backquote,
        KeyCode::Backslash => Code::Backslash,
        KeyCode::BracketLeft => Code::BracketLeft,
        KeyCode::BracketRight => Code::BracketRight,
        KeyCode::Comma => Code::Comma,
        KeyCode::Digit0 => Code::Digit0,
        KeyCode::Digit1 => Code::Digit1,
        KeyCode::Digit2 => Code::Digit2,
        KeyCode::Digit3 => Code::Digit3,
        KeyCode::Digit4 => Code::Digit4,
        KeyCode::Digit5 => Code::Digit5,
        KeyCode::Digit6 => Code::Digit6,
        KeyCode::Digit7 => Code::Digit7,
        KeyCode::Digit8 => Code::Digit8,
        KeyCode::Digit9 => Code::Digit9,
        KeyCode::Equal => Code::Equal,
        KeyCode::KeyA => Code::KeyA,
        KeyCode::KeyB => Code::KeyB,
        KeyCode::KeyC => Code::KeyC,
        KeyCode::KeyD => Code::KeyD,
        KeyCode::KeyE => Code::KeyE,
        KeyCode::KeyF => Code::KeyF,
        KeyCode::KeyG => Code::KeyG,
        KeyCode::KeyH => Code::KeyH,
        KeyCode::KeyI => Code::KeyI,
        KeyCode::KeyJ => Code::KeyJ,
        KeyCode::KeyK => Code::KeyK,
        KeyCode::KeyL => Code::KeyL,
        KeyCode::KeyM => Code::KeyM,
        KeyCode::KeyN => Code::KeyN,
        KeyCode::KeyO => Code::KeyO,
        KeyCode::KeyP => Code::KeyP,
        KeyCode::KeyQ => Code::KeyQ,
        KeyCode::KeyR => Code::KeyR,
        KeyCode::KeyS => Code::KeyS,
        KeyCode::KeyT => Code::KeyT,
        KeyCode::KeyU => Code::KeyU,
        KeyCode::KeyV => Code::KeyV,
        KeyCode::KeyW => Code::KeyW,
        KeyCode::KeyX => Code::KeyX,
        KeyCode::KeyY => Code::KeyY,
        KeyCode::KeyZ => Code::KeyZ,
        KeyCode::Minus => Code::Minus,
        KeyCode::Period => Code::Period,
        KeyCode::Quote => Code::Quote,
        KeyCode::Semicolon => Code::Semicolon,
        KeyCode::Slash => Code::Slash,
        KeyCode::Backspace => Code::Backspace,
        KeyCode::CapsLock => Code::CapsLock,
        KeyCode::Enter => Code::Enter,
        KeyCode::Space => Code::Space,
        KeyCode::Tab => Code::Tab,
        KeyCode::Delete => Code::Delete,
        KeyCode::End => Code::End,
        KeyCode::Home => Code::Home,
        KeyCode::Insert => Code::Insert,
        KeyCode::PageDown => Code::PageDown,
        KeyCode::PageUp => Code::PageUp,
        KeyCode::PrintScreen => Code::PrintScreen,
        KeyCode::ScrollLock => Code::ScrollLock,
        KeyCode::ArrowDown => Code::ArrowDown,
        KeyCode::ArrowLeft => Code::ArrowLeft,
        KeyCode::ArrowRight => Code::ArrowRight,
        KeyCode::ArrowUp => Code::ArrowUp,
        KeyCode::NumLock => Code::NumLock,
        KeyCode::Numpad0 => Code::Numpad0,
        KeyCode::Numpad1 => Code::Numpad1,
        KeyCode::Numpad2 => Code::Numpad2,
        KeyCode::Numpad3 => Code::Numpad3,
        KeyCode::Numpad4 => Code::Numpad4,
        KeyCode::Numpad5 => Code::Numpad5,
        KeyCode::Numpad6 => Code::Numpad6,
        KeyCode::Numpad7 => Code::Numpad7,
        KeyCode::Numpad8 => Code::Numpad8,
        KeyCode::Numpad9 => Code::Numpad9,
        KeyCode::NumpadAdd => Code::NumpadAdd,
        KeyCode::NumpadDecimal => Code::NumpadDecimal,
        KeyCode::NumpadDivide => Code::NumpadDivide,
        KeyCode::NumpadMultiply => Code::NumpadMultiply,
        KeyCode::NumpadSubtract => Code::NumpadSubtract,
        KeyCode::NumpadEnter => Code::NumpadEnter,
        KeyCode::Escape => Code::Escape,
        KeyCode::F1 => Code::F1,
        KeyCode::F2 => Code::F2,
        KeyCode::F3 => Code::F3,
        KeyCode::F4 => Code::F4,
        KeyCode::F5 => Code::F5,
        KeyCode::F6 => Code::F6,
        KeyCode::F7 => Code::F7,
        KeyCode::F8 => Code::F8,
        KeyCode::F9 => Code::F9,
        KeyCode::F10 => Code::F10,
        KeyCode::F11 => Code::F11,
        KeyCode::F12 => Code::F12,
        KeyCode::Pause => Code::Pause,
        _ => return None,
    })
}

pub type HotkeyTrigger = Arc<dyn Fn() + Send + Sync>;
