//! 贴边藏窗，有点像 QQ：贴边缩进去，鼠标在那趴 500ms 再弹出来

use slint::ComponentHandle;
use slint::winit_030::{winit, WinitWindowAccessor};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::AppLauncher;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DockEdge {
    Left,
    Right,
    Top,
}

#[derive(Debug, Clone, Copy)]
struct DockSnapshot {
    edge: DockEdge,
    restored_x: i32,
    restored_y: i32,
}

#[derive(Debug, Clone, Copy)]
enum AnimComplete {
    Dock(DockSnapshot),
    Undock { final_x: i32, final_y: i32 },
}

#[derive(Debug, Clone, Copy)]
struct SlideAnim {
    from_x: i32,
    from_y: i32,
    to_x: i32,
    to_y: i32,
    started: Instant,
    on_complete: AnimComplete,
}

#[derive(Default)]
struct EdgeAutoHideInner {
    docked: Option<DockSnapshot>,
    animation: Option<SlideAnim>,
    last_pos: Option<(i32, i32)>,
    last_move_at: Option<Instant>,
    hover_since: Option<Instant>,
}

static INNER: Mutex<EdgeAutoHideInner> = Mutex::new(EdgeAutoHideInner {
    docked: None,
    animation: None,
    last_pos: None,
    last_move_at: None,
    hover_since: None,
});

const SNAP_THRESHOLD_PX: i32 = 18;
/// 缩进去之后还露一条边，不然鼠标找不到门
const VISIBLE_STRIP_PX: i32 = 12;
const HOVER_DELAY: Duration = Duration::from_millis(500);
const STABLE_BEFORE_SNAP: Duration = Duration::from_millis(280);
const HOTZONE_PAD_PX: i32 = 8;
const SLIDE_DURATION: Duration = Duration::from_millis(160);

#[cfg(windows)]
fn with_inner<R>(f: impl FnOnce(&mut EdgeAutoHideInner) -> R) -> Option<R> {
    INNER.lock().ok().map(|mut g| f(&mut g))
}

/// 状态清掉就行 窗户位置别动 动画还在跑的话交给 restore_if_docked 收尾
pub fn clear_dock_state() {
    let _ = with_inner(|g| {
        g.docked = None;
        g.animation = None;
        g.hover_since = None;
    });
}

/// 正在贴边/滑动时别去裁工作区，否则会跟动画打架
pub fn blocks_work_area_clamp() -> bool {
    INNER
        .lock()
        .ok()
        .is_some_and(|g| g.docked.is_some() || g.animation.is_some())
}

/// 托盘、热键、关设置前：要是还缩着，直接整窗弹出来（别播动画了）
pub fn restore_if_docked(ui: &AppLauncher) {
    #[cfg(windows)]
    restore_if_docked_impl(ui);
    #[cfg(not(windows))]
    let _ = ui;
}

#[cfg(windows)]
fn restore_if_docked_impl(ui: &AppLauncher) {
    let snap = INNER.lock().ok().and_then(|g| g.docked);
    let Some(snap) = snap else {
        let _ = with_inner(|g| {
            g.animation = None;
            g.hover_since = None;
        });
        return;
    };

    let Some((from, target)) = window_geometry(ui) else {
        clear_dock_state();
        return;
    };
    let (nx, ny) = expanded_and_clamped(snap, target.0, target.1, from.0, from.1);

    set_window_position(ui, nx, ny);
    let _ = with_inner(|g| {
        g.docked = None;
        g.animation = None;
        g.hover_since = None;
        g.last_pos = Some((nx, ny));
        g.last_move_at = Some(Instant::now());
    });
}

pub fn start_watcher(ui_weak: slint::Weak<AppLauncher>) {
    #[cfg(windows)]
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(16));
        let uw = ui_weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = uw.upgrade() {
                tick(&ui);
            }
        });
    });
    #[cfg(not(windows))]
    let _ = ui_weak;
}

#[cfg(windows)]
fn window_geometry(ui: &AppLauncher) -> Option<((i32, i32), (u32, u32))> {
    ui.window()
        .with_winit_window(|w| {
            let pos = w.outer_position().ok()?;
            let size = w.outer_size();
            if size.width == 0 || size.height == 0 {
                return None;
            }
            Some(((pos.x, pos.y), (size.width, size.height)))
        })
        .flatten()
}

#[cfg(windows)]
fn expanded_and_clamped(
    snap: DockSnapshot,
    w_px: u32,
    h_px: u32,
    cx: i32,
    cy: i32,
) -> (i32, i32) {
    let w = w_px as i32;
    let h = h_px as i32;
    let work = crate::win_extra::win::monitor_work_area_at_point(cx + w / 2, cy + h / 2);
    let expanded = expanded_position_for_edge(snap.edge, snap.restored_x, snap.restored_y, w, h, work);
    crate::win_extra::win::clamp_window_top_left_for_size(expanded.0, expanded.1, w, h)
}

#[cfg(windows)]
fn start_slide_to(
    ui: &AppLauncher,
    from_x: i32,
    from_y: i32,
    to_x: i32,
    to_y: i32,
    on_complete: AnimComplete,
) {
    if from_x == to_x && from_y == to_y {
        apply_slide_complete(on_complete);
        set_window_position(ui, to_x, to_y);
        return;
    }
    let _ = with_inner(|g| {
        g.animation = Some(SlideAnim {
            from_x,
            from_y,
            to_x,
            to_y,
            started: Instant::now(),
            on_complete,
        });
    });
    set_window_position(ui, from_x, from_y);
}

#[cfg(windows)]
fn apply_slide_complete(done: AnimComplete) {
    let _ = with_inner(|g| {
        g.animation = None;
        g.hover_since = None;
        match done {
            AnimComplete::Dock(snap) => {
                g.docked = Some(snap);
            }
            AnimComplete::Undock { final_x, final_y } => {
                g.docked = None;
                g.last_pos = Some((final_x, final_y));
                g.last_move_at = Some(Instant::now());
            }
        }
    });
}

#[cfg(windows)]
fn tick_animation(ui: &AppLauncher) -> bool {
    let anim = INNER.lock().ok().and_then(|g| g.animation);
    let Some(anim) = anim else {
        return false;
    };

    let elapsed = anim.started.elapsed();
    let t = (elapsed.as_secs_f32() / SLIDE_DURATION.as_secs_f32()).clamp(0.0, 1.0);
    let e = ease_in_out_cubic(t);
    let x = lerp_i32(anim.from_x, anim.to_x, e);
    let y = lerp_i32(anim.from_y, anim.to_y, e);
    set_window_position(ui, x, y);

    if t < 1.0 {
        return true;
    }

    let done = anim.on_complete;
    apply_slide_complete(done);
    if let AnimComplete::Dock(_) = done {
        let _ = with_inner(|g| {
            g.last_pos = Some((anim.to_x, anim.to_y));
            g.last_move_at = Some(Instant::now());
        });
    }
    true
}

#[cfg(windows)]
fn set_window_position(ui: &AppLauncher, x: i32, y: i32) {
    ui.window().with_winit_window(|w| {
        use winit::dpi::PhysicalPosition;
        let _ = w.set_outer_position(PhysicalPosition::new(x, y));
    });
}

#[cfg(windows)]
fn ease_in_out_cubic(t: f32) -> f32 {
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

#[cfg(windows)]
fn lerp_i32(a: i32, b: i32, t: f32) -> i32 {
    a + ((b - a) as f32 * t).round() as i32
}

#[cfg(windows)]
fn tick(ui: &AppLauncher) {
    if tick_animation(ui) {
        return;
    }

    let enabled = crate::app_state_global_opt()
        .map(|s| s.borrow().config.edge_auto_hide)
        .unwrap_or(false);

    if !enabled {
        if INNER
            .lock()
            .ok()
            .is_some_and(|g| g.docked.is_some() || g.animation.is_some())
        {
            restore_if_docked_impl(ui);
        }
        return;
    }

    let visible = ui
        .window()
        .with_winit_window(|w| w.is_visible().unwrap_or(false))
        .unwrap_or(false);
    let minimized = ui
        .window()
        .with_winit_window(|w| w.is_minimized().unwrap_or(false))
        .unwrap_or(false);
    if !visible || minimized {
        return;
    }

    if crate::main_window_maximized() {
        if INNER.lock().ok().is_some_and(|g| g.docked.is_some()) {
            restore_if_docked_impl(ui);
        }
        return;
    }

    let Some(((x, y), (ww, hh))) = window_geometry(ui) else {
        return;
    };
    let w_px = ww as i32;
    let h_px = hh as i32;
    let work = crate::win_extra::win::monitor_work_area_at_point(x + w_px / 2, y + h_px / 2);
    let cursor = crate::win_extra::win::cursor_screen_physical_position();

    let mut inner = match INNER.lock() {
        Ok(g) => g,
        Err(_) => return,
    };

    if let Some(snap) = inner.docked {
        if cursor_in_dock_hotzone(snap.edge, x, y, w_px, h_px, work, cursor) {
            let now = Instant::now();
            let start = inner.hover_since.get_or_insert(now);
            if now.duration_since(*start) >= HOVER_DELAY {
                let (nx, ny) = expanded_and_clamped(snap, ww, hh, x, y);
                inner.docked = None;
                inner.hover_since = None;
                drop(inner);
                start_slide_to(
                    ui,
                    x,
                    y,
                    nx,
                    ny,
                    AnimComplete::Undock {
                        final_x: nx,
                        final_y: ny,
                    },
                );
            }
        } else {
            inner.hover_since = None;
        }
        return;
    }

    inner.hover_since = None;

    if cursor_over_window(x, y, w_px, h_px, cursor) {
        inner.last_pos = Some((x, y));
        inner.last_move_at = Some(Instant::now());
        return;
    }

    let cur = (x, y);
    if inner.last_pos != Some(cur) {
        inner.last_pos = Some(cur);
        inner.last_move_at = Some(Instant::now());
        return;
    }

    let stable = inner
        .last_move_at
        .is_some_and(|t| t.elapsed() >= STABLE_BEFORE_SNAP);
    if !stable {
        return;
    }

    let Some(edge) = detect_snap_edge(x, y, w_px, work) else {
        return;
    };

    let snap = DockSnapshot {
        edge,
        restored_x: x,
        restored_y: y,
    };
    let (hx, hy) = hidden_position_for_edge(edge, x, y, w_px, h_px, work);
    drop(inner);

    start_slide_to(ui, x, y, hx, hy, AnimComplete::Dock(snap));
}

#[cfg(not(windows))]
fn tick(_ui: &AppLauncher) {}

#[cfg(windows)]
fn detect_snap_edge(
    x: i32,
    y: i32,
    w: i32,
    work: (i32, i32, i32, i32),
) -> Option<DockEdge> {
    let (left, top, right, _) = work;
    let near_left = x <= left + SNAP_THRESHOLD_PX;
    let near_right = x + w >= right - SNAP_THRESHOLD_PX;
    let near_top = y <= top + SNAP_THRESHOLD_PX;

    if near_left && !near_right {
        return Some(DockEdge::Left);
    }
    if near_right && !near_left {
        return Some(DockEdge::Right);
    }
    if near_top && !near_left && !near_right {
        return Some(DockEdge::Top);
    }
    None
}

/// 从缩进态弹出来：整窗进工作区，贴边那条轴尽量保留拖动时的位置
#[cfg(windows)]
fn expanded_position_for_edge(
    edge: DockEdge,
    saved_x: i32,
    saved_y: i32,
    w: i32,
    h: i32,
    work: (i32, i32, i32, i32),
) -> (i32, i32) {
    let (left, top, right, bottom) = work;
    let max_y = bottom.saturating_sub(h).max(top);
    let max_x = right.saturating_sub(w).max(left);
    match edge {
        DockEdge::Left => (left, saved_y.clamp(top, max_y)),
        DockEdge::Right => (right.saturating_sub(w), saved_y.clamp(top, max_y)),
        DockEdge::Top => (saved_x.clamp(left, max_x), top),
    }
}

#[cfg(windows)]
fn hidden_position_for_edge(
    edge: DockEdge,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    work: (i32, i32, i32, i32),
) -> (i32, i32) {
    let (left, top, right, _) = work;
    match edge {
        DockEdge::Left => (left - w + VISIBLE_STRIP_PX, y),
        DockEdge::Right => (right - VISIBLE_STRIP_PX, y),
        DockEdge::Top => (x, top - h + VISIBLE_STRIP_PX),
    }
}

#[cfg(windows)]
fn cursor_over_window(win_x: i32, win_y: i32, win_w: i32, win_h: i32, cursor: (i32, i32)) -> bool {
    let (cx, cy) = cursor;
    cx >= win_x && cx < win_x + win_w && cy >= win_y && cy < win_y + win_h
}

#[cfg(windows)]
fn cursor_in_dock_hotzone(
    edge: DockEdge,
    win_x: i32,
    win_y: i32,
    win_w: i32,
    win_h: i32,
    work: (i32, i32, i32, i32),
    cursor: (i32, i32),
) -> bool {
    let (cx, cy) = cursor;
    let (left, top, right, _) = work;
    let y0 = win_y.saturating_sub(HOTZONE_PAD_PX);
    let y1 = win_y.saturating_add(win_h).saturating_add(HOTZONE_PAD_PX);

    match edge {
        DockEdge::Left => {
            let x0 = left.saturating_sub(HOTZONE_PAD_PX);
            let x1 = left.saturating_add(VISIBLE_STRIP_PX + HOTZONE_PAD_PX);
            cx >= x0 && cx <= x1 && cy >= y0 && cy <= y1
        }
        DockEdge::Right => {
            let x0 = right.saturating_sub(VISIBLE_STRIP_PX + HOTZONE_PAD_PX);
            let x1 = right.saturating_add(HOTZONE_PAD_PX);
            cx >= x0 && cx <= x1 && cy >= y0 && cy <= y1
        }
        DockEdge::Top => {
            let x0 = win_x.saturating_sub(HOTZONE_PAD_PX);
            let x1 = win_x.saturating_add(win_w).saturating_add(HOTZONE_PAD_PX);
            let y0 = top.saturating_sub(HOTZONE_PAD_PX);
            let y1 = top.saturating_add(VISIBLE_STRIP_PX + HOTZONE_PAD_PX);
            cx >= x0 && cx <= x1 && cy >= y0 && cy <= y1
        }
    }
}
