#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app_launcher;
mod config;
mod dnd;
mod edge_auto_hide;
mod everything_search;
mod favicon;
mod custom_hotkey;
#[cfg(windows)]
mod hotkey_mouse_win;
mod icon_loader;
mod models;
mod path_convert;
mod single_instance;
#[cfg(windows)]
mod win_ole_drop;
mod win_extra;

use crate::{
    app_launcher::AppLauncher as AppLauncherLogic,
    config::ConfigManager,
    custom_hotkey::{
        binding_from_storage, binding_label, binding_to_storage, default_capture_hint,
        migrate_legacy_hotkey, register_active, set_capture_suppress_mouse, slint_mouse_button,
        winit_modifiers, winit_physical_key_to_code, HotkeyBinding,
    },
    icon_loader::IconLoader,
    models::{AppInfo, Category, LaunchResult, SkinId, UserConfig},
};
use anyhow::{Context, Result};
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use slint::{ComponentHandle, SharedString, VecModel};
use slint::winit_030::{winit, CustomApplicationHandler, EventResult, WinitWindowAccessor};
use std::collections::{HashMap, HashSet};
use std::cell::RefCell;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{MouseButton, TrayIconBuilder, TrayIconEvent};

slint::include_modules!();

thread_local! {
    static TL_SETTINGS_POPUP: RefCell<Option<SettingsPopup>> = RefCell::new(None);
}

/// UI 线程上的设置窗 Weak 失焦时 hide 用
static SETTINGS_FOCUS_POPUP_WEAK: Mutex<Option<slint::Weak<SettingsPopup>>> =
    Mutex::new(None);
static SETTINGS_FOCUS_WINIT_ID: Mutex<Option<winit::window::WindowId>> = Mutex::new(None);

/// invoke_from_event_loop / 托盘回调要 Send 所以只存 Weak + 裸指针（进程活着就有效）
static APP_GLOBAL_UI_WEAK: Mutex<Option<slint::Weak<AppLauncher>>> = Mutex::new(None);
static APP_STATE_RAW: Mutex<Option<usize>> = Mutex::new(None);
static LAST_WORKING_SET_TRIM_MS: AtomicU64 = AtomicU64::new(0);
static HIDDEN_SESSION_RECLAIMED: AtomicBool = AtomicBool::new(false);
static HOTKEY_CAPTURE_ACTIVE: AtomicBool = AtomicBool::new(false);

/// 无框窗下 is_maximized() 经常撒谎 手动记标志 + 最大化前的几何来回切
static MAIN_WINDOW_MAXIMIZED_TOGGLE: AtomicBool = AtomicBool::new(false);
static MAIN_WINDOW_RESTORE_GEOMETRY: Mutex<
    Option<(winit::dpi::PhysicalPosition<i32>, winit::dpi::PhysicalSize<u32>)>,
> = Mutex::new(None);
static MAIN_WINDOW_WINIT_ID: Mutex<Option<winit::window::WindowId>> = Mutex::new(None);

pub(crate) fn main_window_maximized() -> bool {
    MAIN_WINDOW_MAXIMIZED_TOGGLE.load(Ordering::Relaxed)
}

fn register_globals(ui: slint::Weak<AppLauncher>, app_state: &Rc<RefCell<AppState>>) {
    *APP_GLOBAL_UI_WEAK.lock().unwrap() = Some(ui);
    let ptr = Rc::into_raw(Rc::clone(app_state)) as usize;
    *APP_STATE_RAW.lock().unwrap() = Some(ptr);
}

fn app_state_global() -> Rc<RefCell<AppState>> {
    let addr = APP_STATE_RAW.lock().unwrap().unwrap();
    unsafe {
        Rc::increment_strong_count(addr as *const RefCell<AppState>);
        Rc::from_raw(addr as *const RefCell<AppState>)
    }
}

fn app_state_global_opt() -> Option<Rc<RefCell<AppState>>> {
    let addr = APP_STATE_RAW.lock().ok().and_then(|g| *g)?;
    unsafe {
        Rc::increment_strong_count(addr as *const RefCell<AppState>);
        Some(Rc::from_raw(addr as *const RefCell<AppState>))
    }
}

struct AppState {
    config: UserConfig,
    config_manager: ConfigManager,
    icon_loader: IconLoader,
    launcher: AppLauncherLogic,
    hotkey_mgr: RefCell<Option<GlobalHotKeyManager>>,
    registered_hotkey: Arc<Mutex<Option<HotKey>>>,
    search_cache: RefCell<SearchIndexCache>,
 /// 当前会话内已验证通过、可临时查看内容的分组 id
    unlocked_categories: RefCell<HashSet<String>>,
}

#[derive(Default)]
struct SearchIndexCache {
    fingerprint: u64,
    by_id: HashMap<String, SearchIndexData>,
}

struct SearchIndexData {
    name_lc: String,
    path_lc: String,
    keywords_lc: Vec<String>,
    web_url_lc: Option<String>,
}

struct FileDropHandler {
    state: Rc<RefCell<AppState>>,
    ui_weak: Rc<RefCell<Option<slint::Weak<AppLauncher>>>>,
    modifiers: RefCell<winit::keyboard::ModifiersState>,
}

impl CustomApplicationHandler for FileDropHandler {
    fn window_event(
        &mut self,
        _event_loop: &winit::event_loop::ActiveEventLoop,
        _window_id: winit::window::WindowId,
        _winit_window: Option<&winit::window::Window>,
        _slint_window: Option<&slint::Window>,
        event: &winit::event::WindowEvent,
    ) -> EventResult {
        if matches!(event, winit::event::WindowEvent::Focused(false)) {
            let main_id = MAIN_WINDOW_WINIT_ID.lock().ok().and_then(|g| *g);
            if main_id == Some(_window_id) {
                let ui_weak = self.ui_weak.borrow().clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.and_then(|w| w.upgrade()) {
                        ui.invoke_blur_search_input();
                    }
                });
            }
            let settings_id = SETTINGS_FOCUS_WINIT_ID.lock().ok().and_then(|g| *g);
            if settings_id == Some(_window_id) {
                let ui_weak = self.ui_weak.borrow().clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.and_then(|w| w.upgrade()) {
 // 打开主窗口模态（快捷键等）时先保留设置窗，待模态关闭后再关
                        if main_window_has_modal_overlay(&ui) {
                            return;
                        }
                        dismiss_settings_if_main_focused_without_modal(&ui);
                        if settings_popup_is_visible() {
                            hide_settings_popup();
                        }
                    } else if settings_popup_is_visible() {
                        hide_settings_popup();
                    }
                });
            }
            return EventResult::Propagate;
        }

        if matches!(event, winit::event::WindowEvent::Focused(true)) {
            let settings_id = SETTINGS_FOCUS_WINIT_ID.lock().ok().and_then(|g| *g);
            if settings_popup_is_visible() && settings_id != Some(_window_id) {
                let ui_weak = self.ui_weak.borrow().clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.and_then(|w| w.upgrade()) {
                        dismiss_settings_if_main_focused_without_modal(&ui);
                    }
                });
            }
            return EventResult::Propagate;
        }

        let uw = self.ui_weak.borrow().clone();
        let Some(uw) = uw else {
            return EventResult::Propagate;
        };

 // winit 在事件循环线程投递窗口事件，与 Slint UI 同线程 此处同步调用避免
 // `invoke_from_event_loop` 返回 Err 时被静默忽略导致拖放无任何效果
        if let winit::event::WindowEvent::ModifiersChanged(m) = event {
            *self.modifiers.borrow_mut() = m.state();
        }

        if HOTKEY_CAPTURE_ACTIVE.load(Ordering::Relaxed) {
            if let winit::event::WindowEvent::KeyboardInput { event, .. } = event {
                use winit::event::ElementState;
                use winit::keyboard::{KeyCode, PhysicalKey};
                if event.state == ElementState::Pressed {
                    if matches!(event.physical_key, PhysicalKey::Code(KeyCode::Escape)) {
                        end_hotkey_capture(&uw);
                        return EventResult::Propagate;
                    }
                    let mods = winit_modifiers(*self.modifiers.borrow());
                    if let Some(code) = winit_physical_key_to_code(event.physical_key) {
                        if !custom_hotkey::is_modifier_code(code) {
                            let hk = HotKey::new(Some(mods), code);
                            apply_hotkey_binding(&self.state, &uw, HotkeyBinding::Keyboard(hk));
                            return EventResult::Propagate;
                        }
                    }
                }
            }
        }

        match event {
            winit::event::WindowEvent::ScaleFactorChanged { .. } => {
 // 勿在此 request_inner_size：与 winit WM_DPICHANGED 建议矩形抢尺寸
 // 易造成 OpenGL 表面与客户区不同步 → 鼠标命中相对画面整体偏移
                let settings_id = SETTINGS_FOCUS_WINIT_ID.lock().ok().and_then(|g| *g);
                if settings_id == Some(_window_id) {
                    return EventResult::Propagate;
                }
                let main_id = MAIN_WINDOW_WINIT_ID.lock().ok().and_then(|g| *g);
                if main_id != Some(_window_id) {
                    return EventResult::Propagate;
                }
                if let Some(ui) = uw.upgrade() {
                    let ui_w = ui.as_weak();
                    let opacity = app_state_global_opt()
                        .map(|s| s.borrow().config.window_opacity)
                        .unwrap_or(100);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            apply_rounded_corners_ui(&ui);
                            apply_window_opacity_ui(&ui, opacity);
                            clamp_main_window_to_monitor_work_area(&ui);
                            ui.window().request_redraw();
                        }
                    });
                }
            }
            winit::event::WindowEvent::HoveredFile(_) => {
                if let Some(ui) = uw.upgrade() {
                    ui.set_drop_active(true);
                    ui.set_status_text(SharedString::from(
                        "松开鼠标即可添加（支持 exe · lnk · url · 文本里的 http(s) 链接 · 文件夹 · 图片 · 文档）",
                    ));
                }
            }
            winit::event::WindowEvent::HoveredFileCancelled => {
                if let Some(ui) = uw.upgrade() {
                    ui.set_drop_active(false);
                    ui.set_status_text(SharedString::from(
                        "拖入文件添加 · 左键启动 · 右键菜单",
                    ));
                }
            }
            winit::event::WindowEvent::DroppedFile(path) => {
                let path = path.clone();
                if let Some(ui) = uw.upgrade() {
                    ui.set_drop_active(false);
                    add_dropped_path(&ui, &self.state, path);
                } else {
                    let ptr = Rc::into_raw(Rc::clone(&self.state)) as usize;
                    let u = uw.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        let rc = unsafe { Rc::from_raw(ptr as *const RefCell<AppState>) };
                        if let Some(ui) = u.upgrade() {
                            ui.set_drop_active(false);
                            add_dropped_path(&ui, &rc, path);
                        }
                        drop(rc);
                    });
                }
            }
            _ => {}
        }
        EventResult::Propagate
    }
}

fn main() -> Result<()> {
 // 必须在热键/托盘/主窗创建之前：否则 winit 的 become_dpi_aware 可能已失败
 // Windows 会对窗口做位图拉伸，表现为光标与按钮命中区上下错位
    #[cfg(windows)]
    crate::win_extra::win::ensure_per_monitor_dpi_v2();

    let Some(_single_instance) = single_instance::try_first_instance()? else {
        return Ok(());
    };

    let config_manager = ConfigManager::new()?;
    let mut config = config_manager.load_or_create_config_sync()?;
    normalize_legacy_zero_sort_orders(&mut config);
    migrate_legacy_hotkey(&mut config.hotkey);
    let icon_loader = IconLoader::new(config_manager.cache_dir())?;
    let launcher = AppLauncherLogic::new();

    let hotkey_mgr = GlobalHotKeyManager::new().ok();
    let registered_hotkey = Arc::new(Mutex::new(None));

    let app_state = Rc::new(RefCell::new(AppState {
        config,
        config_manager,
        icon_loader,
        launcher,
        hotkey_mgr: RefCell::new(hotkey_mgr),
        registered_hotkey: registered_hotkey.clone(),
        search_cache: RefCell::new(SearchIndexCache::default()),
        unlocked_categories: RefCell::new(HashSet::new()),
    }));

    let ui_handle: Rc<RefCell<Option<slint::Weak<AppLauncher>>>> =
        Rc::new(RefCell::new(None));
    slint::BackendSelector::new()
        .backend_name("winit".into())
 // 禁用 winit 自带的 RegisterDragDrop（仅 HDROP），否则会与自定义 IDropTarget 抢占同一 HWND
 // 导致记事本等纯文本 OLE 拖放始终落到禁止光标
        .with_winit_window_attributes_hook(|mut attrs| {
            #[cfg(windows)]
            {
                use slint::winit_030::winit::platform::windows::WindowAttributesExtWindows;
                attrs = attrs.with_drag_and_drop(false);
                attrs = attrs.with_skip_taskbar(true);
            }
            attrs
        })
        .with_winit_custom_application_handler(FileDropHandler {
            state: Rc::clone(&app_state),
            ui_weak: Rc::clone(&ui_handle),
            modifiers: RefCell::new(winit::keyboard::ModifiersState::default()),
        })
        .select()?;

    let ui = AppLauncher::new()?;
    *ui_handle.borrow_mut() = Some(ui.as_weak());
    register_globals(ui.as_weak(), &app_state);

    let first_cat_id = app_state
        .borrow()
        .config
        .categories
        .first()
        .map(|c| c.id.clone())
        .unwrap_or_default();

    update_ui_categories(&ui, &app_state.borrow().config.categories);
    ui.set_selected_category(SharedString::from(first_cat_id.clone()));
    {
        let st = app_state.borrow();
        if category_requires_unlock(&st, &first_cat_id) {
            ui.set_filtered_apps(Rc::new(VecModel::from(Vec::<AppItem>::new())).into());
            ui.set_password_dialog_mode(1);
            ui.set_password_dialog_cat_id(SharedString::from(first_cat_id.as_str()));
            if let Some(cat) = st
                .config
                .categories
                .iter()
                .find(|c| c.id == first_cat_id)
            {
                ui.set_password_dialog_cat_name(SharedString::from(cat.name.as_str()));
            }
            ui.set_password_input(SharedString::from(""));
            ui.set_password_error(SharedString::from(""));
            ui.set_show_password_dialog(true);
            ui.set_status_text(SharedString::from("该分组已密码保护，请输入密码查看"));
        } else {
            refresh_apps(&ui, &st, &first_cat_id, "");
            ui.set_status_text(SharedString::from(
                "就绪：可拖入 exe · lnk · url · 文本链接 · 文件夹 · 图片 · 文档",
            ));
        }
    }

    sync_settings_ui(&ui, &app_state.borrow());

    ui.set_app_version(SharedString::from(env!("CARGO_PKG_VERSION")));

    apply_rounded_corners_ui(&ui);
    apply_window_opacity_ui(&ui, app_state.borrow().config.window_opacity);
    register_main_window_tracking(&ui);
    let uw_round = ui.as_weak();
    let startup_opacity = app_state.borrow().config.window_opacity;
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(160));
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = uw_round.upgrade() {
                apply_rounded_corners_ui(&ui);
                apply_window_opacity_ui(&ui, startup_opacity);
                // 双击启动后直接把键盘焦点放进搜索框，省掉手动点一下
                reset_search_for_invoke(&ui);
            }
        });
    });

    let exe_path = std::env::current_exe().unwrap_or_default();
    let _ = crate::win_extra::win::set_auto_start(app_state.borrow().config.auto_start, &exe_path);

    let ui_weak_hotkey = ui.as_weak();
    let hk_registered = app_state.borrow().registered_hotkey.clone();
    GlobalHotKeyEvent::set_event_handler(Some(move |e: GlobalHotKeyEvent| {
        if e.state != HotKeyState::Pressed {
            return;
        }
        let guard = match hk_registered.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let Some(hk) = guard.as_ref() else {
            return;
        };
        if e.id != hk.id {
            return;
        }
        drop(guard);
        let uw = ui_weak_hotkey.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = uw.upgrade() {
                toggle_window_visible(&ui);
            }
        });
    }));

    if let Err(e) = apply_hotkey_from_config(&app_state) {
        ui.set_status_text(SharedString::from(format!("快捷键：{}", e)));
    }

    #[cfg(windows)]
    {
        let uw_trigger = ui.as_weak();
        hotkey_mouse_win::set_trigger(Arc::new(move || {
            let uw = uw_trigger.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = uw.upgrade() {
                    toggle_window_visible(&ui);
                }
            });
        }));
    }

    let open_settings = setup_callbacks(&ui, Rc::clone(&app_state));

    let help_path = app_state
        .borrow()
        .config_manager
        .config_dir_path()
        .join("USAGE.txt");
    let _tray = build_tray_menu(ui.as_weak(), help_path, open_settings).context("托盘图标")?;

    if app_state.borrow().config.always_on_top {
        apply_always_on_top(&ui, true);
    }

    let start_min = app_state.borrow().config.start_minimized;
    let uw = ui.as_weak();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(120));
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = uw.upgrade() {
                if start_min {
                    ui.window().with_winit_window(|w| {
                        w.set_minimized(true);
                    });
                }
            }
        });
    });

    ui.show().context("显示主窗口")?;
    edge_auto_hide::start_watcher(ui.as_weak());
    #[cfg(windows)]
    {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        fn mount_ole_if_win32(
            ui: &AppLauncher,
            uiw: slint::Weak<AppLauncher>,
            st: &Rc<RefCell<AppState>>,
        ) {
            ui.window().with_winit_window(|w| {
                if let Ok(h) = w.window_handle() {
                    if let RawWindowHandle::Win32(rw) = h.as_raw() {
                        if let Err(e) = win_ole_drop::mount_launcher_ole_drop_target(
                            rw.hwnd.get() as isize,
                            uiw,
                            Rc::clone(st),
                        ) {
                            eprintln!("挂载扩展拖放：{}", e);
                        }
                    }
                }
            });
        }

        let uiw = ui.as_weak();
        let st = Rc::clone(&app_state);
        mount_ole_if_win32(&ui, uiw.clone(), &st);

 // OpenGL 子 HWND 常在首帧/create_window 之后才挂到树上 延迟再挂一轮避免「文件与文本拖放全无」
        for delay_ms in [240u64, 720] {
            let uiw2 = uiw.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = uiw2.upgrade() {
                        let st = app_state_global();
                        mount_ole_if_win32(&ui, ui.as_weak(), &st);
                    }
                });
            });
        }
    }
    ui.window()
        .on_close_requested(|| slint::CloseRequestResponse::HideWindow);
    slint::run_event_loop_until_quit().context("事件循环")?;
    Ok(())
}

fn slint_image_from_file(path: &Path) -> Option<slint::Image> {
    if let Ok(img) = slint::Image::load_from_path(path) {
        return Some(img);
    }
    let bytes = std::fs::read(path).ok()?;
    let img = image::load_from_memory(&bytes).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Some(slint::Image::from_rgba8(
        slint::SharedPixelBuffer::clone_from_slice(rgba.as_raw(), w, h),
    ))
}

fn resolved_background_path(st: &AppState) -> Option<PathBuf> {
    let stored = st.config.background_image.as_ref()?;
    let path = crate::path_convert::resolve_path(
        Path::new(&st.config_manager.config_dir_path()),
        stored,
    );
    if path.is_file() {
        Some(path)
    } else {
        None
    }
}

fn background_image_for_config(st: &AppState) -> slint::Image {
    resolved_background_path(st)
        .and_then(|p| slint_image_from_file(&p))
        .unwrap_or_default()
}

fn apply_background_image_to_ui(ui: &AppLauncher, st: &AppState) {
    let active = resolved_background_path(st).is_some();
    ui.set_has_custom_background(active);
    ui.set_background_image(if active {
        background_image_for_config(st)
    } else {
        slint::Image::default()
    });
}

fn crop_square_rgba(rgba: &image::RgbaImage) -> image::RgbaImage {
    let (w, h) = rgba.dimensions();
    let side = w.min(h).max(1);
    let x = (w.saturating_sub(side)) / 2;
    let y = (h.saturating_sub(side)) / 2;
    image::imageops::crop_imm(rgba, x, y, side, side).to_image()
}

fn sync_settings_ui(ui: &AppLauncher, st: &AppState) {
    let cfg = &st.config;
    ui.set_skin_index(cfg.skin.as_index());
    ui.set_cfg_always_on_top(cfg.always_on_top);
    ui.set_cfg_auto_start(cfg.auto_start);
    ui.set_cfg_start_minimized(cfg.start_minimized);
    ui.set_cfg_edge_auto_hide(cfg.edge_auto_hide);
    ui.set_cfg_hide_on_launch_click(cfg.hide_on_launch_click);
    ui.set_icon_size(cfg.icon_size as f32);
    sync_hotkey_ui_props(ui, cfg);
    ui.set_launcher_title(SharedString::from(cfg.launcher_title.clone()));
    apply_background_image_to_ui(ui, st);
}

fn hotkey_display_label(cfg: &UserConfig) -> String {
    if !cfg.hotkey_enabled {
        return String::from("（已禁用）");
    }
    binding_from_storage(&cfg.hotkey)
        .map(|b| binding_label(&b))
        .unwrap_or_else(|| String::from("（未设置）"))
}

fn sync_hotkey_ui_props(ui: &AppLauncher, cfg: &UserConfig) {
    ui.set_hotkey_enabled(cfg.hotkey_enabled);
    ui.set_hotkey_label(SharedString::from(hotkey_display_label(cfg)));
    ui.set_hotkey_capturing(false);
}

fn begin_hotkey_capture(ui_weak: &slint::Weak<AppLauncher>) {
    HOTKEY_CAPTURE_ACTIVE.store(true, Ordering::Relaxed);
    set_capture_suppress_mouse(true);
    if let Some(ui) = ui_weak.upgrade() {
        ui.set_hotkey_capturing(true);
        ui.set_hotkey_label(SharedString::from(default_capture_hint()));
    }
}

fn end_hotkey_capture(ui_weak: &slint::Weak<AppLauncher>) {
    HOTKEY_CAPTURE_ACTIVE.store(false, Ordering::Relaxed);
    set_capture_suppress_mouse(false);
    if let Some(ui) = ui_weak.upgrade() {
        ui.set_hotkey_capturing(false);
        let cfg = app_state_global().borrow().config.clone();
        ui.set_hotkey_label(SharedString::from(hotkey_display_label(&cfg)));
    }
}

fn apply_hotkey_binding(
    state: &Rc<RefCell<AppState>>,
    ui_weak: &slint::Weak<AppLauncher>,
    binding: HotkeyBinding,
) {
    {
        let mut st = state.borrow_mut();
        st.config.hotkey = Some(binding_to_storage(&binding));
        if !st.config.hotkey_enabled {
            st.config.hotkey_enabled = true;
        }
    }
    end_hotkey_capture(ui_weak);
    if let Err(e) = apply_hotkey_from_config(state) {
        if let Some(ui) = ui_weak.upgrade() {
            ui.set_status_text(SharedString::from(format!("快捷键：{}", e)));
        }
        return;
    }
    if let Some(ui) = ui_weak.upgrade() {
        let cfg = state.borrow().config.clone();
        sync_hotkey_ui_props(&ui, &cfg);
        ui.set_status_text(SharedString::from("快捷键已更新"));
    }
    let st = state.borrow();
    let cm = st.config_manager.clone();
    let cfg = st.config.clone();
    let _ = cm.save_config_sync(&cfg);
}

fn resolve_app_path(cm: &ConfigManager, app: &AppInfo) -> PathBuf {
    crate::path_convert::resolve_path(Path::new(&cm.config_dir_path()), &app.path)
}

fn build_app_item(app: &AppInfo, cm: &ConfigManager, icon_loader: &IconLoader) -> AppItem {
    let p = resolve_app_path(cm, app);
    let base = cm.config_dir_path();
    let icon = icon_loader.load_app_icon(app, &p, Path::new(&base));
    AppItem {
        id: SharedString::from(app.id.clone()),
        name: SharedString::from(app.name.clone()),
        icon,
        path: SharedString::from(app.path.to_string_lossy().to_string()),
        category_id: SharedString::from(app.category_id.clone()),
        args: SharedString::from(app.args.join(" ")),
        sort_order: app.sort_order,
        pinned: app.pinned,
    }
}

fn apps_in_category_sorted<'a>(apps: &'a [AppInfo], cat_id: &str) -> Vec<&'a AppInfo> {
    let mut v: Vec<&AppInfo> = apps.iter().filter(|a| a.category_id == cat_id).collect();
    v.sort_by(|a, b| compare_apps(a, b));
    v
}

/// 当前分组内的显示序号（1 = 最前），与网格所见顺序一致
fn display_sort_index_in_category(apps: &[AppInfo], app_id: &str, cat_id: &str) -> i32 {
    apps_in_category_sorted(apps, cat_id)
        .iter()
        .position(|a| a.id == app_id)
        .map(|i| (i + 1) as i32)
        .unwrap_or(1)
}

fn assign_sort_orders_from_display_order(apps: &mut [AppInfo], cat_id: &str, ordered_ids: &[String]) {
    let n = ordered_ids.len();
    for (rank, id) in ordered_ids.iter().enumerate() {
        if let Some(app) = apps.iter_mut().find(|a| a.id == *id && a.category_id == cat_id) {
            app.sort_order = (n.saturating_sub(1 + rank)) as i32;
        }
    }
 // 分组内置顶项的实际顺序由 pin_order 决定，手动改排序后需同步
    let mut pin_rank = 1u64;
    for id in ordered_ids {
        if let Some(app) = apps.iter_mut().find(|a| a.id == *id && a.category_id == cat_id) {
            if app.pinned {
                app.pin_order = pin_rank;
                pin_rank += 1;
            }
        }
    }
}

fn apply_display_sort_index(apps: &mut [AppInfo], app_id: &str, cat_id: &str, display_index_1based: i32) {
    let mut ids: Vec<String> = apps_in_category_sorted(apps, cat_id)
        .into_iter()
        .map(|a| a.id.clone())
        .collect();
    let Some(cur) = ids.iter().position(|id| id == app_id) else {
        return;
    };
    let id = ids.remove(cur);
    let insert_at = (display_index_1based.max(1) as usize - 1).min(ids.len());
    ids.insert(insert_at, id);
    assign_sort_orders_from_display_order(apps, cat_id, &ids);
}

fn next_sort_order_for_new_app(apps: &[AppInfo], cat_id: &str) -> i32 {
    let in_cat = apps_in_category_sorted(apps, cat_id);
    if in_cat.is_empty() {
        return 0;
    }
    in_cat.iter().map(|a| a.sort_order).min().unwrap_or(0).saturating_sub(1)
}

/// 旧配置里同组 sort_order 全为 0 时，按当前展示顺序写入序号，便于编辑对话框显示 1、2、3…
fn normalize_legacy_zero_sort_orders(config: &mut crate::models::UserConfig) {
    let cat_ids: Vec<String> = config.categories.iter().map(|c| c.id.clone()).collect();
    for cat_id in cat_ids {
        let in_cat: Vec<&AppInfo> = config
            .apps
            .iter()
            .filter(|a| a.category_id == cat_id)
            .collect();
        if in_cat.len() <= 1 {
            continue;
        }
        if !in_cat.iter().all(|a| a.sort_order == 0) {
            continue;
        }
        let ids: Vec<String> = {
            let mut v = in_cat;
            v.sort_by(|a, b| compare_apps(a, b));
            v.into_iter().map(|a| a.id.clone()).collect()
        };
        assign_sort_orders_from_display_order(&mut config.apps, &cat_id, &ids);
    }
}

fn compare_apps(a: &AppInfo, b: &AppInfo) -> std::cmp::Ordering {
 // 置顶项永远在前 多个置顶按“置顶操作先后”从早到晚排列
    b.pinned
        .cmp(&a.pinned)
        .then_with(|| {
            if a.pinned && b.pinned {
                a.pin_order.cmp(&b.pin_order)
            } else {
                std::cmp::Ordering::Equal
            }
        })
 // 用户手动排序优先于使用频率
        .then_with(|| b.sort_order.cmp(&a.sort_order))
        .then_with(|| b.exec_count.cmp(&a.exec_count))
        .then_with(|| a.name.cmp(&b.name))
}

fn sort_apps_by_usage(apps: &mut [AppInfo]) {
    apps.sort_by(compare_apps);
}

fn sort_apps_for_display(apps: &mut [AppInfo]) {
    apps.sort_by(|a, b| {
        a.category_id
            .cmp(&b.category_id)
            .then_with(|| b.pinned.cmp(&a.pinned))
            .then_with(|| {
                if a.pinned && b.pinned {
                    a.pin_order.cmp(&b.pin_order)
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .then_with(|| b.exec_count.cmp(&a.exec_count))
            .then_with(|| a.sort_order.cmp(&b.sort_order))
            .then_with(|| a.name.cmp(&b.name))
    });
}

/// 与左侧分组列表顺序一致：用于 `selected-category` 为空时的默认分组（与 `add_dropped_path` 回落逻辑对齐）
fn category_fallback_id(state: &AppState) -> String {
    state
        .config
        .categories
        .iter()
        .min_by_key(|c| c.sort_order)
        .map(|c| c.id.clone())
        .unwrap_or_default()
}

fn category_exists(state: &AppState, cat_id: &str) -> bool {
    !cat_id.is_empty() && state.config.categories.iter().any(|c| c.id == cat_id)
}

fn category_name_by_id(state: &AppState, cat_id: &str) -> String {
    state
        .config
        .categories
        .iter()
        .find(|c| c.id == cat_id)
        .map(|c| c.name.clone())
        .unwrap_or_else(|| cat_id.to_string())
}

fn resolve_category_id_input(state: &AppState, input: &str) -> String {
    if category_exists(state, input) {
        return input.to_string();
    }
    state
        .config
        .categories
        .iter()
        .find(|c| c.name == input)
        .map(|c| c.id.clone())
        .unwrap_or_else(|| input.to_string())
}

fn apps_search_fingerprint(apps: &[AppInfo]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    apps.len().hash(&mut h);
    for a in apps {
        a.id.hash(&mut h);
        a.name.hash(&mut h);
        a.path.hash(&mut h);
        a.keywords.hash(&mut h);
        a.web_url.hash(&mut h);
    }
    h.finish()
}

fn ensure_search_cache(state: &AppState) {
    let fp = apps_search_fingerprint(&state.config.apps);
    let mut cache = state.search_cache.borrow_mut();
    if cache.fingerprint == fp {
        return;
    }
    let mut next_by_id: HashMap<String, SearchIndexData> =
        HashMap::with_capacity(state.config.apps.len());
    for a in &state.config.apps {
        let mut keywords_lc = Vec::with_capacity(a.keywords.len());
        for k in &a.keywords {
            keywords_lc.push(k.to_lowercase());
        }
        next_by_id.insert(
            a.id.clone(),
            SearchIndexData {
                name_lc: a.name.to_lowercase(),
                path_lc: a.path.to_string_lossy().to_lowercase(),
                keywords_lc,
                web_url_lc: a.web_url.as_ref().map(|w| w.to_lowercase()),
            },
        );
    }
    cache.by_id = next_by_id;
    cache.fingerprint = fp;
}

#[inline]
fn app_matches_query(app: &AppInfo, q: &str, cache: &SearchIndexCache) -> bool {
    if q.is_empty() {
        return true;
    }
 // 使用运行时小写索引，避免每次搜索重复 to_lowercase 匹配语义与原逻辑一致
    if let Some(ix) = cache.by_id.get(&app.id) {
        ix.name_lc.contains(q)
            || ix.path_lc.contains(q)
            || ix.keywords_lc.iter().any(|k| k.contains(q))
            || ix.web_url_lc.as_ref().map(|w| w.contains(q)).unwrap_or(false)
    } else {
 // 兜底（理论上不会命中）
        app.name.to_lowercase().contains(q)
            || app.path.to_string_lossy().to_lowercase().contains(q)
            || app.keywords.iter().any(|k| k.to_lowercase().contains(q))
            || app
                .web_url
                .as_ref()
                .map(|w| w.to_lowercase().contains(q))
                .unwrap_or(false)
    }
}

fn category_requires_unlock(state: &AppState, cat_id: &str) -> bool {
    let Some(cat) = state.config.categories.iter().find(|c| c.id == cat_id) else {
        return false;
    };
    cat.has_password() && !state.unlocked_categories.borrow().contains(cat_id)
}

fn refresh_apps(ui: &AppLauncher, state: &AppState, selected_category: &str, search: &str) -> usize {
    let cm = &state.config_manager;
    let cat_filter_owned = if !category_exists(state, selected_category) {
        category_fallback_id(state)
    } else {
        selected_category.to_string()
    };
    let cat_filter = cat_filter_owned.as_str();

    if category_requires_unlock(state, cat_filter) {
        ui.set_filtered_apps(Rc::new(VecModel::from(Vec::<AppItem>::new())).into());
        return 0;
    }

    let mut sorted_refs: Vec<&AppInfo> = state.config.apps.iter().collect();
    sorted_refs.sort_by(|a, b| compare_apps(a, b));

    let q = search.to_lowercase();
    let mut items: Vec<AppItem> = Vec::with_capacity(sorted_refs.len());
    if q.is_empty() {
 // 非搜索状态不持有搜索索引，降低长期常驻内存
        let mut cache = state.search_cache.borrow_mut();
        cache.by_id.clear();
        cache.fingerprint = 0;
        drop(cache);
        for a in sorted_refs.iter().copied() {
            if a.category_id != cat_filter {
                continue;
            }
            items.push(build_app_item(a, cm, &state.icon_loader));
        }
    } else {
        ensure_search_cache(state);
        let cache = state.search_cache.borrow();
        for a in sorted_refs.iter().copied() {
            if a.category_id != cat_filter || !app_matches_query(a, &q, &cache) {
                continue;
            }
            items.push(build_app_item(a, cm, &state.icon_loader));
        }
    }
    let count = items.len();
    ui.set_filtered_apps(Rc::new(VecModel::from(items)).into());
    if !q.is_empty() && count > 0 {
 // 搜索结果刷新后顶对齐首条（过滤列表中索引 0 即首个命中）
        ui.set_apps_viewport_y(0.0);
    }
    count
}

fn apply_everything_hits(ui: &AppLauncher, hits: &[everything_search::EverythingHit]) {
    let mut items: Vec<FileHitItem> = Vec::with_capacity(hits.len());
    for (i, hit) in hits.iter().enumerate() {
        let path_str = hit.path.to_string_lossy().to_string();
        items.push(FileHitItem {
            id: SharedString::from(format!("ev:{i}")),
            name: SharedString::from(hit.name.as_str()),
            path: SharedString::from(path_str),
            location: SharedString::from(hit.location.as_str()),
            modified: SharedString::from(hit.modified.as_str()),
            attributes: SharedString::from(hit.attributes.as_str()),
        });
    }
    let count = items.len();
 // 计算机搜索用列表，清空图标网格
    ui.set_filtered_apps(Rc::new(VecModel::from(Vec::<AppItem>::new())).into());
    ui.set_file_hits(Rc::new(VecModel::from(items)).into());
    if count > 0 {
        ui.set_file_hits_viewport_y(0.0);
    }
}

fn clear_file_hits(ui: &AppLauncher) {
    ui.set_file_hits(Rc::new(VecModel::from(Vec::<FileHitItem>::new())).into());
    ui.set_file_hits_viewport_y(0.0);
}

/// 唤起窗口后把键盘焦点交给搜索框。
/// 窗口刚 show 出来时系统焦点尚未落定，立刻 focus 有可能被吞掉，所以补一次延迟重试。
fn focus_search_input(ui: &AppLauncher) {
    ui.invoke_focus_search_input();
    let ui_weak = ui.as_weak();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(90));
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.invoke_focus_search_input();
            }
        });
    });
}

/// 热键/托盘唤起窗口时清掉上次的搜索残留，让用户可以直接开打。
/// 本来就没有搜索内容时只补焦点，不动状态栏文案（避免盖掉密码弹窗提示）。
fn reset_search_for_invoke(ui: &AppLauncher) {
    let had_search = !ui.get_search_text().is_empty() || !ui.get_search_input_text().is_empty();
    if had_search {
        ui.set_search_text(SharedString::from(""));
        ui.set_search_input_text(SharedString::from(""));
        clear_file_hits(ui);
        if let Some(state) = app_state_global_opt() {
            let cat = ui.get_selected_category().to_string();
            let st = state.borrow();
            refresh_apps(ui, &st, &cat, "");
        }
        if !ui.get_show_password_dialog() {
            ui.set_status_text(SharedString::from(
                "就绪：可拖入 exe · lnk · url · 文本链接 · 文件夹 · 图片 · 文档",
            ));
        }
    }
    focus_search_input(ui);
}

fn parse_color(s: &str) -> slint::Brush {
    if s.starts_with('#') && s.len() == 7 {
        let r = u8::from_str_radix(&s[1..3], 16).unwrap_or(74);
        let g = u8::from_str_radix(&s[3..5], 16).unwrap_or(130);
        let b = u8::from_str_radix(&s[5..7], 16).unwrap_or(196);
        slint::Brush::from(slint::Color::from_rgb_u8(r, g, b))
    } else {
        slint::Brush::from(slint::Color::from_rgb_u8(74, 130, 196))
    }
}

fn update_ui_categories(ui: &AppLauncher, categories: &[Category]) {
    let mut sorted: Vec<Category> = categories.to_vec();
    sorted.sort_by_key(|c| c.sort_order);
    let items: Vec<CategoryItem> = sorted
        .iter()
        .map(|c| CategoryItem {
            id: SharedString::from(c.id.clone()),
            name: SharedString::from(c.name.clone()),
            color: parse_color(&c.color),
            sort_order: c.sort_order,
            has_password: c.has_password(),
        })
        .collect();
    ui.set_categories(Rc::new(VecModel::from(items)).into());
}

#[cfg(windows)]
fn apply_rounded_corners_ui(ui: &AppLauncher) {
    let skin = ui.get_skin_index();
    ui.window().with_winit_window(|win| {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        if let Ok(h) = win.window_handle() {
            if let RawWindowHandle::Win32(w) = h.as_raw() {
                crate::win_extra::win::apply_rounded_corners(w.hwnd.get() as isize, skin);
            }
        }
    });
}

#[cfg(not(windows))]
fn apply_rounded_corners_ui(_ui: &AppLauncher) {}

#[cfg(windows)]
fn apply_window_opacity_ui(ui: &AppLauncher, percent: u8) {
    let pct = percent.clamp(50, 100);
    ui.window().with_winit_window(|win| {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        if let Ok(h) = win.window_handle() {
            if let RawWindowHandle::Win32(w) = h.as_raw() {
                crate::win_extra::win::apply_window_opacity(w.hwnd.get() as isize, pct);
            }
        }
    });
}

#[cfg(not(windows))]
fn apply_window_opacity_ui(_ui: &AppLauncher, _percent: u8) {}

/// DWM / Z 序 / 换背景之后 layered alpha 容易丢 从配置再刷一遍
fn reapply_main_window_opacity_from_config(ui: &AppLauncher) {
    let pct = app_state_global_opt()
        .map(|s| s.borrow().config.window_opacity)
        .unwrap_or(100);
    apply_window_opacity_ui(ui, pct);
}

/// set_window_level 是异步的 同步改透明度会被后面的 SetWindowPos 冲掉 所以延后刷
fn schedule_reapply_main_window_opacity(ui: &AppLauncher) {
    let opacity = app_state_global_opt()
        .map(|s| s.borrow().config.window_opacity)
        .unwrap_or(100);
    let uw = ui.as_weak();
    let _ = slint::invoke_from_event_loop({
        let uw = uw.clone();
        move || {
            if let Some(ui) = uw.upgrade() {
                apply_window_opacity_ui(&ui, opacity);
            }
        }
    });
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(80));
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = uw.upgrade() {
                apply_window_opacity_ui(&ui, opacity);
            }
        });
    });
}

fn register_main_window_tracking(ui: &AppLauncher) {
    ui.window().with_winit_window(|w| {
        if let Ok(mut id) = MAIN_WINDOW_WINIT_ID.lock() {
            *id = Some(w.id());
        }
    });
}

#[cfg(windows)]
fn apply_rounded_corners_settings(popup: &SettingsPopup) {
    let skin = popup.get_skin_index();
    popup.window().with_winit_window(|win| {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        if let Ok(h) = win.window_handle() {
            if let RawWindowHandle::Win32(w) = h.as_raw() {
                crate::win_extra::win::apply_rounded_corners(w.hwnd.get() as isize, skin);
            }
        }
    });
}

#[cfg(not(windows))]
fn apply_rounded_corners_settings(_popup: &SettingsPopup) {}

fn apply_always_on_top(ui: &AppLauncher, on: bool) {
    use winit::window::WindowLevel;
    ui.window().with_winit_window(|w| {
        if on {
            w.set_window_level(WindowLevel::AlwaysOnTop);
        } else {
            w.set_window_level(WindowLevel::Normal);
        }
    });
    schedule_reapply_main_window_opacity(ui);
}

fn main_window_has_focus(ui: &AppLauncher) -> bool {
    let mut focused = false;
    ui.window().with_winit_window(|w| {
        focused = w.has_focus();
    });
    focused
}

fn main_window_has_modal_overlay(ui: &AppLauncher) -> bool {
    ui.get_show_edit_dialog()
        || ui.get_show_hotkey_dialog()
        || ui.get_show_about_dialog()
        || ui.get_show_delete_category_dialog()
        || ui.get_show_category_dialog()
        || ui.get_show_password_dialog()
        || ui.get_show_menu()
        || ui.get_show_category_tab_menu()
}

fn hide_settings_popup() {
    *SETTINGS_FOCUS_WINIT_ID.lock().unwrap() = None;
    let pw = SETTINGS_FOCUS_POPUP_WEAK.lock().unwrap().clone();
    if let Some(p) = pw.and_then(|x| x.upgrade()) {
        let _ = p.window().hide();
    }
    restore_main_window_level_from_config();
}

fn settings_popup_is_visible() -> bool {
    SETTINGS_FOCUS_POPUP_WEAK
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .and_then(|w| w.upgrade())
        .map_or(false, |p| p.window().is_visible())
}

/// 主窗口已获得焦点且没有模态层时，关闭仍显示中的设置窗（点击主界面空白处等）
fn dismiss_settings_if_main_focused_without_modal(ui: &AppLauncher) {
    if !settings_popup_is_visible() {
        return;
    }
    if main_window_has_modal_overlay(ui) {
        return;
    }
    if main_window_has_focus(ui) {
        hide_settings_popup();
    }
}

fn apply_settings_popup_always_on_top(popup: &SettingsPopup) {
    use winit::window::WindowLevel;
    popup.window().with_winit_window(|w| {
        w.set_window_level(WindowLevel::AlwaysOnTop);
    });
}

fn set_main_window_level_normal(ui: &AppLauncher) {
    use winit::window::WindowLevel;
    ui.window().with_winit_window(|w| {
        w.set_window_level(WindowLevel::Normal);
    });
    schedule_reapply_main_window_opacity(ui);
}

fn restore_main_window_level_from_config() {
    let ui_weak = APP_GLOBAL_UI_WEAK.lock().ok().and_then(|g| g.clone());
    let Some(ui_weak) = ui_weak else {
        return;
    };
    let Some(ui) = ui_weak.upgrade() else {
        return;
    };
    let state = app_state_global();
    let on = state.borrow().config.always_on_top;
    apply_always_on_top(&ui, on);
}

/// 热键/托盘切主窗显示 最小化时 is_visible 还是 true 得单独看
/// 跟标题栏关窗一样必须走 Slint hide/show 别只用 winit set_visible
/// 否则第二个实例唤起来只有透明壳不渲染
fn toggle_window_visible(ui: &AppLauncher) {
    let vis = ui
        .window()
        .with_winit_window(|w| w.is_visible().unwrap_or(false))
        .unwrap_or(false);
    let minimized = ui
        .window()
        .with_winit_window(|w| w.is_minimized().unwrap_or(false))
        .unwrap_or(false);
    if vis && !minimized {
        edge_auto_hide::restore_if_docked(ui);
        let _ = ui.window().hide();
        compact_runtime_caches();
    } else {
        show_main_window(ui);
        ui.window().with_winit_window(|w| {
            w.focus_window();
        });
        reset_search_for_invoke(ui);
    }
}

fn toggle_main_window_from_tray(ui: &AppLauncher) {
    toggle_window_visible(ui);
}

/// 临时 topmost 抬一下再还原 不是永久置顶
fn activate_main_window_once(ui: &AppLauncher) {
    use winit::window::WindowLevel;

    let keep_always_on_top = app_state_global_opt()
        .map(|s| s.borrow().config.always_on_top)
        .unwrap_or(false);

    ui.window().with_winit_window(|w| {
        let visible = w.is_visible().unwrap_or(false);
        if !visible {
            return;
        }
        let target_level = if keep_always_on_top {
            WindowLevel::AlwaysOnTop
        } else {
            WindowLevel::Normal
        };
        w.set_window_level(WindowLevel::AlwaysOnTop);
        w.focus_window();
        w.set_window_level(target_level);
    });
    schedule_reapply_main_window_opacity(ui);
}

fn hide_main_window(ui: &AppLauncher) {
    edge_auto_hide::restore_if_docked(ui);
    let _ = ui.window().hide();
    compact_runtime_caches();
}

#[cfg(windows)]
fn pick_image_as_background() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .add_filter("Image", &["png", "jpg", "jpeg", "webp", "bmp"])
        .pick_file()
}

#[cfg(not(windows))]
fn pick_image_as_background() -> Option<PathBuf> {
    None
}

fn bg_overlay_rgba_for_skin(skin: SkinId) -> (u8, u8, u8, u8) {
    match skin {
        SkinId::Dark => (12, 18, 28, 178),
        SkinId::Light => (245, 247, 250, 170),
        SkinId::Warm => (44, 34, 28, 168),
        SkinId::Cozy => (26, 22, 34, 168),
    }
}

fn save_background_image_from_path(state: &mut AppState, src: &Path) -> Result<PathBuf> {
    state.config_manager.ensure_dirs()?;
    let dest = state.config_manager.background_image_path();
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = std::fs::read(src)?;
    let img = image::load_from_memory(&bytes)
        .with_context(|| format!("无法解码图片：{}", src.display()))?;
    let cropped = crop_square_rgba(&img.to_rgba8());
    let resized = image::imageops::resize(
        &cropped,
        1024,
        1024,
        image::imageops::FilterType::Lanczos3,
    );
    resized
        .save(&dest)
        .with_context(|| format!("无法写入：{}", dest.display()))?;
    Ok(state.config_manager.background_image_rel_path())
}

#[cfg(windows)]
fn pick_png_for_custom_icon() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .add_filter("PNG 图片", &["png"])
        .pick_file()
}

#[cfg(not(windows))]
fn pick_png_for_custom_icon() -> Option<PathBuf> {
    None
}

fn save_custom_icon_from_path(state: &mut AppState, app_id: &str, src: &Path) -> Result<PathBuf> {
    state.config_manager.ensure_dirs()?;
    let dest = state.config_manager.custom_icon_path(app_id);
    let bytes = std::fs::read(src)?;
    let img = image::load_from_memory(&bytes)?;
    let cropped = crop_square_rgba(&img.to_rgba8());
    let resized = image::imageops::resize(
        &cropped,
        256,
        256,
        image::imageops::FilterType::Lanczos3,
    );
    resized.save(&dest)?;
    Ok(state.config_manager.custom_icon_rel_path(app_id))
}

fn launch_single_app(state: &mut AppState, app_id: &str) -> bool {
    let Some(idx) = state.config.apps.iter().position(|a| a.id == app_id) else {
        return false;
    };
    state.config.apps[idx].exec_count += 1;
    state.config.apps[idx].last_used = Some(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    );
    let mut app = state.config.apps[idx].clone();
    app.path = resolve_app_path(&state.config_manager, &app);
    matches!(state.launcher.launch(&app), LaunchResult::Success)
}

fn launch_all_apps_in_category(state: &mut AppState, cat_id: &str) -> (usize, usize) {
    let ids: Vec<String> = apps_in_category_sorted(&state.config.apps, cat_id)
        .into_iter()
        .map(|a| a.id.clone())
        .collect();
    let mut ok = 0usize;
    let mut fail = 0usize;
    for id in ids {
        if launch_single_app(state, &id) {
            ok += 1;
        } else {
            fail += 1;
        }
    }
    sort_apps_by_usage(&mut state.config.apps);
    (ok, fail)
}

fn show_main_window(ui: &AppLauncher) {
    edge_auto_hide::restore_if_docked(ui);
    let _ = ui.window().show();
 // 进入前台后重置“本轮隐藏态已回收”标记，下次再隐藏时允许回收
    HIDDEN_SESSION_RECLAIMED.store(false, Ordering::Relaxed);
    clamp_main_window_to_monitor_work_area(ui);
    ui.window().with_winit_window(|w| {
        let _ = w.set_minimized(false);
    });
    schedule_reapply_main_window_opacity(ui);
    ui.window().request_redraw();
}

fn usage_txt_path() -> PathBuf {
    crate::config::application_root_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("USAGE.txt")
}

fn open_usage_txt_file() {
    let path = usage_txt_path();
    if !path.is_file() {
        return;
    }
    let _ = std::process::Command::new("notepad").arg(&path).spawn();
}

fn open_usage_txt_with_status(ui: &AppLauncher) {
    let path = usage_txt_path();
    if !path.is_file() {
        ui.set_status_text(SharedString::from(format!(
            "未找到使用说明，请将 USAGE.txt 放在程序同目录：{}",
            path.display()
        )));
        show_main_window(ui);
        return;
    }
    let _ = std::process::Command::new("notepad").arg(&path).spawn();
}

fn compact_runtime_caches() {
    let Some(state_rc) = app_state_global_opt() else {
        return;
    };
 // 稳定性优先：避免在 UI 线程上因 RefCell 冲突触发 panic（无控制台时表现为“闪退”）
    let state_borrow = state_rc.try_borrow();
    if let Ok(state) = state_borrow {
        state.icon_loader.trim_memo_to(64);
        if let Ok(mut cache) = state.search_cache.try_borrow_mut() {
            cache.by_id.clear();
            cache.fingerprint = 0;
        }
    }
    maybe_trim_working_set_hidden();
}

fn maybe_trim_working_set_hidden() {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
 // 规则：
 // 1) 每次隐藏阶段只回收一次（不反复回收）
 // 2) 仅当曾显示过主窗口后，下一次隐藏才允许回收
 // 3) 两次回收最少间隔 30 秒
    const COOLDOWN_MS: u64 = 30_000;
    const DELAY_MS: u64 = 1200;
    if HIDDEN_SESSION_RECLAIMED.load(Ordering::Relaxed) {
        return;
    }
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let last = LAST_WORKING_SET_TRIM_MS.load(Ordering::Relaxed);
    if now_ms.saturating_sub(last) < COOLDOWN_MS {
        return;
    }
    if LAST_WORKING_SET_TRIM_MS
        .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    HIDDEN_SESSION_RECLAIMED.store(true, Ordering::Relaxed);
    let _ = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(DELAY_MS));
        crate::win_extra::win::trim_current_process_working_set();
    });
}

fn periodic_memory_reclaim_tick(ui: &AppLauncher) {
    let vis = ui
        .window()
        .with_winit_window(|w| w.is_visible().unwrap_or(false))
        .unwrap_or(false);
    let minimized = ui
        .window()
        .with_winit_window(|w| w.is_minimized().unwrap_or(false))
        .unwrap_or(false);
    if !vis || minimized {
 // 窗口隐藏/最小化：做完整回收
        compact_runtime_caches();
    }
}

fn start_periodic_memory_reclaim(ui_weak: slint::Weak<AppLauncher>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(30));
        let uw = ui_weak.clone();
        let r = slint::invoke_from_event_loop(move || {
            if let Some(ui) = uw.upgrade() {
                periodic_memory_reclaim_tick(&ui);
            }
        });
        if r.is_err() {
            break;
        }
    });
}

#[cfg(windows)]
fn clamp_main_window_to_monitor_work_area(ui: &AppLauncher) {
    if edge_auto_hide::blocks_work_area_clamp() {
        return;
    }
    ui.window().with_winit_window(|win| {
        use winit::dpi::PhysicalPosition;
        let Ok(pos) = win.outer_position() else {
            return;
        };
        let sz = win.outer_size();
        let w = sz.width as i32;
        let h = sz.height as i32;
        if w <= 0 || h <= 0 {
            return;
        }
        let mut target = crate::win_extra::win::clamp_window_top_left_for_size(pos.x, pos.y, w, h);
        if let Some(monitor) = win.current_monitor().or_else(|| win.primary_monitor()) {
            let msize = monitor.size();
            let mpos = monitor.position();
            let mw = msize.width as i32;
            let mh = msize.height as i32;
            if mw > 0 && mh > 0 {
                let max_x = mpos.x + mw - w;
                let max_y = mpos.y + mh - h;
                target.0 = target.0.clamp(mpos.x, max_x.max(mpos.x));
                target.1 = target.1.clamp(mpos.y, max_y.max(mpos.y));
            }
        }
        if target.0 != pos.x || target.1 != pos.y {
            let _ = win.set_outer_position(PhysicalPosition::new(target.0, target.1));
        }
    });
}

#[cfg(not(windows))]
fn clamp_main_window_to_monitor_work_area(_ui: &AppLauncher) {}

/// 设置窗左上角往光标附近塞 outer_w/h 是 winit 外框物理像素
#[cfg(windows)]
fn settings_popup_top_left_for_outer_size(outer_w: i32, outer_h: i32) -> (i32, i32) {
    let (cx, cy) = crate::win_extra::win::cursor_screen_physical_position();
    let candidates = [
        (cx, cy),                 // 左上角在鼠标
        (cx, cy - outer_h),      // 左下角在鼠标
        (cx - outer_w, cy - outer_h), // 右下角在鼠标
    ];
    let mut chosen = (cx, cy);
    let mut found = false;
    for (x, y) in candidates {
        let (nx, ny) =
            crate::win_extra::win::clamp_window_top_left_for_size(x, y, outer_w, outer_h);
        if nx == x && ny == y {
            chosen = (x, y);
            found = true;
            break;
        }
    }
    if !found {
        chosen = crate::win_extra::win::clamp_window_top_left_for_size(cx, cy, outer_w, outer_h);
    }
    chosen
}

/// show 之前先把坐标写进 WindowAttributes 创建 HWND 时就在光标旁
/// 不然会闪一帧「先默认位置再跳」
#[cfg(windows)]
fn prime_settings_popup_position_before_show(popup: &SettingsPopup) {
    let mut sf = popup.window().scale_factor();
    if sf <= 0.0 {
        sf = 1.0;
    }
    let outer_w = (336.0_f32 * sf).round().max(1.0) as i32;
    let layout_h = popup.get_layout_height();
    let outer_h = (layout_h * sf).round().max(1.0) as i32;
    let (x, y) = settings_popup_top_left_for_outer_size(outer_w, outer_h);
    popup
        .window()
        .set_position(slint::PhysicalPosition::new(x, y));
}

#[cfg(not(windows))]
fn prime_settings_popup_position_before_show(_popup: &SettingsPopup) {}

#[cfg(windows)]
fn position_settings_popup_near_cursor(popup: &SettingsPopup) {
 // 不再强制 request_inner_size：SettingsPopup 高度由 slint 表达式自动计算
 // 若这里写死高度会导致托盘入口打开时被拉长
    popup.window().with_winit_window(|win| {
        use winit::dpi::PhysicalPosition;

        let outer = win.outer_size();
        let scale = win.scale_factor();
        let (w, h) = if outer.width > 0 && outer.height > 0 {
            (outer.width.max(1) as i32, outer.height.max(1) as i32)
        } else {
            let wf = (336.0 * scale).round().max(1.0) as i32;
            let hf = (win.inner_size().height as f64).round().max(1.0) as i32;
            (wf, hf)
        };

        let chosen = settings_popup_top_left_for_outer_size(w, h);
        let _ = win.set_outer_position(PhysicalPosition::new(chosen.0, chosen.1));
    });
}

#[cfg(not(windows))]
fn position_settings_popup_near_cursor(popup: &SettingsPopup) {
    let _ = popup;
}

fn sync_settings_to_popup(popup: &SettingsPopup, st: &AppState) {
    let cfg = &st.config;
    popup.set_skin_index(cfg.skin.as_index());
    popup.set_cfg_always_on_top(cfg.always_on_top);
    popup.set_cfg_auto_start(cfg.auto_start);
    popup.set_cfg_start_minimized(cfg.start_minimized);
    popup.set_cfg_edge_auto_hide(cfg.edge_auto_hide);
    popup.set_cfg_hide_on_launch_click(cfg.hide_on_launch_click);
    popup.set_icon_size(cfg.icon_size as f32);
    popup.set_window_opacity(cfg.window_opacity as f32);
    popup.set_launcher_title(SharedString::from(cfg.launcher_title.clone()));
    popup.set_background_image(SharedString::from(
        if cfg.background_image.is_some() {
            "已设置自定义背景"
        } else {
            "未设置"
        },
    ));
}

fn apply_hotkey_from_config(state: &Rc<RefCell<AppState>>) -> Result<(), String> {
    let mut st = state.borrow_mut();
    migrate_legacy_hotkey(&mut st.config.hotkey);
    let enabled = st.config.hotkey_enabled;
    let binding = binding_from_storage(&st.config.hotkey);
    let hk_store = st.registered_hotkey.clone();
    let mut kb_slot = hk_store
        .lock()
        .map_err(|_| String::from("热键状态锁定失败"))?;
    let Some(mgr) = st.hotkey_mgr.get_mut().as_mut() else {
        return Err(String::from("热键管理器不可用"));
    };
    register_active(mgr, &mut kb_slot, enabled, binding.as_ref())
}

fn tray_icon_from_app_ico_png() -> Result<tray_icon::Icon> {
    let mut paths: Vec<PathBuf> = vec![PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("ico.png")];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            paths.push(dir.join("ico.png"));
        }
    }
    for p in paths {
        let Ok(bytes) = std::fs::read(&p) else {
            continue;
        };
        let Ok(img) = image::load_from_memory(&bytes) else {
            continue;
        };
        let rgba = img.to_rgba8();
        const SIZE: u32 = 64;
        let resized = if rgba.width() == SIZE && rgba.height() == SIZE {
            rgba
        } else {
            image::imageops::resize(&rgba, SIZE, SIZE, image::imageops::FilterType::Lanczos3)
        };
        return tray_icon::Icon::from_rgba(resized.into_raw(), SIZE, SIZE).context("托盘图标 RGBA");
    }
    let embedded = include_bytes!("../ico.png");
    let img = image::load_from_memory(embedded).context("解码嵌入的托盘图标（ico.png）")?;
    let rgba = img.to_rgba8();
    const SIZE: u32 = 64;
    let resized = if rgba.width() == SIZE && rgba.height() == SIZE {
        rgba
    } else {
        image::imageops::resize(&rgba, SIZE, SIZE, image::imageops::FilterType::Lanczos3)
    };
    tray_icon::Icon::from_rgba(resized.into_raw(), SIZE, SIZE).context("托盘图标 RGBA（嵌入）")
}

fn fallback_blue_tray_icon() -> Result<tray_icon::Icon> {
    let mut rgba = vec![0u8; 64 * 64 * 4];
    for px in rgba.chunks_exact_mut(4) {
        px[0] = 70;
        px[1] = 125;
        px[2] = 195;
        px[3] = 255;
    }
    tray_icon::Icon::from_rgba(rgba, 64, 64).context("托盘占位图标")
}

fn build_tray_menu(
    ui_weak: slint::Weak<AppLauncher>,
    help_path: PathBuf,
    open_settings: Arc<dyn Fn() + Send + Sync>,
) -> Result<tray_icon::TrayIcon> {
    let icon = tray_icon_from_app_ico_png().or_else(|_| fallback_blue_tray_icon())?;

    let menu = Menu::new();
    menu.append(&MenuItem::with_id("show", "显示/隐藏主窗口", true, None))?;
    menu.append(&MenuItem::with_id("settings", "设置", true, None))?;
    menu.append(&MenuItem::with_id("help", "使用说明", true, None))?;
    menu.append(&MenuItem::with_id("quit", "退出程序", true, None))?;

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip("亦安快速启动")
        .with_icon(icon)
        .build()
        .context("创建托盘")?;

    let ui_menu = ui_weak.clone();
    let open_settings_menu = Arc::clone(&open_settings);
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        match e.id.as_ref() {
            "show" => {
                let uw = ui_menu.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = uw.upgrade() {
                        toggle_main_window_from_tray(&ui);
                    }
                });
            }
            "settings" => {
                let f = Arc::clone(&open_settings_menu);
                let _ = slint::invoke_from_event_loop(move || {
                    f();
                });
            }
            "help" => {
                let path = help_path.clone();
                let uw = ui_menu.clone();
                if path.is_file() {
                    let _ = std::thread::spawn(move || {
                        let _ = std::process::Command::new("notepad").arg(&path).spawn();
                    });
                } else {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = uw.upgrade() {
                            ui.set_status_text(SharedString::from(format!(
                                "未找到使用说明，请将 USAGE.txt 放在程序同目录：{}",
                                path.display()
                            )));
                            show_main_window(&ui);
                        }
                    });
                }
            }
            "quit" => {
                let _ = slint::quit_event_loop();
            }
            _ => {}
        }
    }));

    let ui_tray = ui_weak.clone();
    TrayIconEvent::set_event_handler(Some(move |ev: TrayIconEvent| {
        match ev {
            TrayIconEvent::Click { button, .. } if button == MouseButton::Left => {
                let uw = ui_tray.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = uw.upgrade() {
                        activate_main_window_once(&ui);
                    }
                });
            }
            TrayIconEvent::DoubleClick { button, .. } if button == MouseButton::Left => {
                let uw = ui_tray.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = uw.upgrade() {
                        toggle_main_window_from_tray(&ui);
                    }
                });
            }
            _ => {}
        }
    }));

    Ok(tray)
}

fn parse_internet_shortcut_url(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    for raw in text.lines() {
        let line = raw.trim().trim_start_matches('\u{feff}');
        if line.is_empty() {
            continue;
        }
        let upper = line.to_ascii_uppercase();
        if upper.starts_with("URL=") {
            let u = line.get(4..).unwrap_or("").trim();
            if !u.is_empty() {
                return Some(u.to_string());
            }
        }
    }
    None
}

#[cfg(windows)]
fn resolve_windows_shortcut_target(path: &Path) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::{Interface, PCWSTR};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, IPersistFile, STGM, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink, SLGP_RAWPATH};

    fn wide_from_path(p: &Path) -> Vec<u16> {
        p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
    }

    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        let shell_link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        let persist: IPersistFile = shell_link.cast().ok()?;

        let wpath = wide_from_path(path);
        persist.Load(PCWSTR(wpath.as_ptr()), STGM(0)).ok()?;

        let mut buf = vec![0u16; 32768];
        shell_link
            .GetPath(&mut buf, std::ptr::null_mut(), SLGP_RAWPATH.0 as u32)
            .ok()?;
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        if end == 0 {
            return None;
        }
        let s = String::from_utf16(&buf[..end]).ok()?;
        let t = s.trim();
        if t.is_empty() {
            None
        } else {
            Some(PathBuf::from(t))
        }
    }
}

#[cfg(not(windows))]
fn resolve_windows_shortcut_target(_path: &Path) -> Option<PathBuf> {
    None
}

/// 从纯文本里抠第一个 http(s) URL 跟拖 .url 一个待遇
/// 只看 trim 后前 256 字 免得长文里误匹配 记事本拖进来一般就在这段里
pub(crate) fn extract_http_url_from_text(s: &str) -> Option<String> {
    const MAX_LOOKAHEAD_CHARS: usize = 256;
    let t = s.trim_start();
    let head_end = t
        .char_indices()
        .nth(MAX_LOOKAHEAD_CHARS)
        .map(|(i, _)| i)
        .unwrap_or(t.len());
    let head = &t[..head_end];
    let lower = head.to_ascii_lowercase();
    let pos = lower.find("https://").or_else(|| lower.find("http://"))?;
    let tail = &t[pos..];
    let end = tail
        .find(|c: char| {
            c.is_whitespace()
                || matches!(
                    c,
                    '"' | '\'' | '<' | '>' | ')' | ']' | '}' | '，' | '。' | '｜' | '|' | '`'
                )
        })
        .unwrap_or(tail.len());
    let url = tail[..end].trim_end_matches(|c: char| {
        matches!(c, '.' | ',' | ';' | '。' | ')' | ']' | '}' | '，')
    });
    if url.len() < 8 {
        return None;
    }
    let parsed = url::Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    Some(url.to_string())
}

pub(crate) fn add_dropped_http_url(
    ui: &AppLauncher,
    app_state: &Rc<RefCell<AppState>>,
    url: String,
) {
    let url = url.trim().to_owned();
    let Some(parsed) = url::Url::parse(&url)
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https"))
    else {
        ui.set_status_text(SharedString::from("添加失败：不是有效的 http(s) 链接"));
        return;
    };

    let dedupe_key = url.to_lowercase();

    let mut state = app_state.borrow_mut();
    if state.config.apps.iter().any(|a| {
        a.web_url
            .as_ref()
            .map(|w| w.to_lowercase())
            .is_some_and(|w| w == dedupe_key)
            || a.path.to_string_lossy().to_lowercase() == dedupe_key
    }) {
        ui.set_status_text(SharedString::from("该项目已在列表中"));
        return;
    }

    let name = parsed
        .host_str()
        .map(|h| h.to_string())
        .unwrap_or_else(|| "网页".to_string());

    let selected_category = ui.get_selected_category().to_string();
    let has_valid_selection = category_exists(&state, &selected_category);
    let category_id = if !has_valid_selection {
        category_fallback_id(&state)
    } else {
        selected_category
    };

    let path = PathBuf::from(url.clone());
    let mut app = AppInfo::new(AppInfo::generate_id(), name.clone(), path, category_id.clone());
    app.web_url = Some(url.clone());
    app.keywords.push(String::from("url"));
    app.keywords.push(String::from("http"));
    app.keywords.push(String::from("https"));
    app.keywords.push(String::from("internet"));
    app.keywords.push(String::from("website"));
    if let Some(h) = parsed.host_str() {
        app.keywords.push(h.to_lowercase());
    }

    let fetch_web = app.web_url.clone();
    let new_app_id = app.id.clone();
    app.sort_order = next_sort_order_for_new_app(&state.config.apps, &category_id);

    state.config.apps.push(app);
    sort_apps_by_usage(&mut state.config.apps);

    if !has_valid_selection && !category_id.is_empty() {
        ui.set_selected_category(SharedString::from(category_id.as_str()));
    }

    ui.set_search_text(SharedString::from(""));
    ui.set_search_input_text(SharedString::from(""));
    let cat_now = ui.get_selected_category().to_string();
    refresh_apps(ui, &state, &cat_now, "");

    let count = state.config.apps.len();
    ui.set_status_text(SharedString::from(format!("已添加网页：{}（共 {} 项）", name, count)));

    let cm = state.config_manager.clone();
    let cfg = state.config.clone();
    let _ = cm.save_config_sync(&cfg);
    drop(state);

    if let Some(web) = fetch_web {
        if let Some(bytes) = crate::favicon::download_site_icon_png(&web) {
            let mut state = app_state.borrow_mut();
            let _ = state.config_manager.ensure_dirs();
            let dest = state.config_manager.get_icon_cache_path(&new_app_id);
            if std::fs::write(&dest, &bytes).is_ok() {
                if let Some(app) = state.config.apps.iter_mut().find(|a| a.id == new_app_id) {
                    app.icon_path = Some(dest.clone());
                    let cm = state.config_manager.clone();
                    let cfg = state.config.clone();
                    let _ = cm.save_config_sync(&cfg);
                    state.icon_loader.invalidate_memo_path(&dest);
                }
            }
            drop(state);
            let cat_now = ui.get_selected_category().to_string();
            refresh_apps(ui, &app_state.borrow(), &cat_now, "");
        }
    }
}

pub(crate) fn add_dropped_path(ui: &AppLauncher, app_state: &Rc<RefCell<AppState>>, path: PathBuf) {
    if !path.exists() {
        ui.set_status_text(SharedString::from("添加失败：路径不存在"));
        return;
    }

    let mut state = app_state.borrow_mut();
 // 过滤启动器外拖临时目录中的 .lnk，避免“拖出后又拖回窗口”被误识别为新增项目
    let dragout_dir = state.config_manager.cache_dir().join("dragout");
    if path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("lnk"))
        && path.starts_with(&dragout_dir)
    {
        ui.set_status_text(SharedString::from("已取消外部拖拽创建"));
        return;
    }
    let source_ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase());
    let mut resolved_path = path.clone();
    if source_ext.as_deref() == Some("lnk") {
        if let Some(target) = resolve_windows_shortcut_target(&path) {
            if !target.as_os_str().is_empty() {
                resolved_path = target;
            }
        }
    }
    let path_key = resolved_path.to_string_lossy().to_lowercase();

    if state.config.apps.iter().any(|a| a.path.to_string_lossy().to_lowercase() == path_key) {
        ui.set_status_text(SharedString::from("该项目已在列表中"));
        return;
    }

    let name = resolved_path
        .file_stem()
        .or_else(|| resolved_path.file_name())
        .and_then(|n| n.to_str())
        .filter(|n| !n.trim().is_empty())
        .unwrap_or("未命名")
        .to_string();

    let selected_category = ui.get_selected_category().to_string();
    let has_valid_selection = category_exists(&state, &selected_category);
    let category_id = if !has_valid_selection {
        category_fallback_id(&state)
    } else {
        selected_category
    };

    let mut app = AppInfo::new(
        AppInfo::generate_id(),
        name.clone(),
        resolved_path.clone(),
        category_id.clone(),
    );
    if let Some(el) = source_ext.as_ref() {
        app.keywords.push(el.clone());
        if el == "url" {
            app.keywords.push(String::from("internet"));
            app.keywords.push(String::from("website"));
            if let Some(u) = parse_internet_shortcut_url(&path) {
                app.web_url = Some(u.clone());
                if let Ok(parsed) = url::Url::parse(u.trim()) {
                    if let Some(h) = parsed.host_str() {
                        app.keywords.push(h.to_lowercase());
                    }
                }
            }
        }
    }
    if resolved_path.is_dir() {
        app.keywords.push(String::from("folder"));
    }

    let fetch_web = app.web_url.clone();
    let new_app_id = app.id.clone();
    app.sort_order = next_sort_order_for_new_app(&state.config.apps, &category_id);

    state.config.apps.push(app);
    sort_apps_by_usage(&mut state.config.apps);

    if !has_valid_selection && !category_id.is_empty() {
        ui.set_selected_category(SharedString::from(category_id.as_str()));
    }

 // 清空搜索，否则新项目可能被当前关键字过滤掉而不出现在列表中
    ui.set_search_text(SharedString::from(""));
    ui.set_search_input_text(SharedString::from(""));
    let cat_now = ui.get_selected_category().to_string();
    refresh_apps(ui, &state, &cat_now, "");

    let count = state.config.apps.len();
    ui.set_status_text(SharedString::from(format!("已添加：{}（共 {} 项）", name, count)));

    let cm = state.config_manager.clone();
    let cfg = state.config.clone();
    let _ = cm.save_config_sync(&cfg);
    drop(state);

    if let Some(url) = fetch_web {
        if let Some(bytes) = crate::favicon::download_site_icon_png(&url) {
            let mut state = app_state.borrow_mut();
            let _ = state.config_manager.ensure_dirs();
            let dest = state.config_manager.get_icon_cache_path(&new_app_id);
            if std::fs::write(&dest, &bytes).is_ok() {
                if let Some(app) = state.config.apps.iter_mut().find(|a| a.id == new_app_id) {
                    app.icon_path = Some(dest.clone());
                    let cm = state.config_manager.clone();
                    let cfg = state.config.clone();
                    let _ = cm.save_config_sync(&cfg);
                    state.icon_loader.invalidate_memo_path(&dest);
                }
            }
            drop(state);
            let cat_now = ui.get_selected_category().to_string();
            refresh_apps(ui, &app_state.borrow(), &cat_now, "");
        }
    }
}

/// `insert_before`：插入到该槽位之前（0..=len），与界面横杠含义一致
fn reorder_categories_vec(categories: &mut Vec<Category>, from: usize, insert_before: usize) -> bool {
    let n = categories.len();
    if n <= 1 || from >= n || insert_before > n {
        return false;
    }
    categories.sort_by_key(|c| c.sort_order);
 // 拖到相邻槽（视觉上不动）
    if insert_before == from || insert_before == from + 1 {
        return false;
    }
    let item = categories.remove(from);
    let insert_at = if insert_before > from {
        insert_before - 1
    } else {
        insert_before
    };
    let insert_at = insert_at.min(categories.len());
    categories.insert(insert_at, item);
    for (i, c) in categories.iter_mut().enumerate() {
        c.sort_order = i as i32;
    }
    true
}

fn wire_settings_popup_handlers(
    popup: &SettingsPopup,
    ui_weak: slint::Weak<AppLauncher>,
    app_state: Rc<RefCell<AppState>>,
) {
    let popup_weak = popup.as_weak();

    popup.on_settings_toggle_always_on_top({
        let ui_weak = ui_weak.clone();
        let state = app_state.clone();
        let popup_weak = popup_weak.clone();
        move || {
            let (v, cm, cfg) = {
                let mut st = state.borrow_mut();
                st.config.always_on_top = !st.config.always_on_top;
                let v = st.config.always_on_top;
                (v, st.config_manager.clone(), st.config.clone())
            };
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_cfg_always_on_top(v);
                if popup_weak.upgrade().is_some() {
                    set_main_window_level_normal(&ui);
                } else {
                    apply_always_on_top(&ui, v);
                }
            }
            if let Some(p) = popup_weak.upgrade() {
                p.set_cfg_always_on_top(v);
 // 主窗口切换 topmost 后，立即把设置窗重新拉回最上层并聚焦
 // 避免主窗口覆盖设置窗
                apply_settings_popup_always_on_top(&p);
                p.window().with_winit_window(|w| {
                    w.focus_window();
                });
            }
            let _ = cm.save_config_sync(&cfg);
            if let Some(ui) = ui_weak.upgrade() {
                let cat = ui.get_selected_category().to_string();
                let search = ui.get_search_text().to_string();
                refresh_apps(&ui, &state.borrow(), &cat, &search);
            }
        }
    });

    popup.on_settings_save_launcher_title({
        let ui_weak = ui_weak.clone();
        let state = app_state.clone();
        let popup_weak = popup_weak.clone();
        move |name: SharedString| {
            let mut s = name.to_string();
            s = s.trim().to_string();
            if s.is_empty() {
                s = String::from("亦安");
            }
            let mut st = state.borrow_mut();
            st.config.launcher_title = s.clone();
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_launcher_title(SharedString::from(s.clone()));
            }
            if let Some(p) = popup_weak.upgrade() {
                p.set_launcher_title(SharedString::from(s));
            }
            let cm = st.config_manager.clone();
            let cfg = st.config.clone();
            let _ = cm.save_config_sync(&cfg);
            if let Some(ui) = ui_weak.upgrade() {
                let cat = ui.get_selected_category().to_string();
                let search = ui.get_search_text().to_string();
                refresh_apps(&ui, &st, &cat, &search);
            }
        }
    });

    popup.on_settings_set_icon_size({
        let ui_weak = ui_weak.clone();
        let state = app_state.clone();
        move |v: f32| {
            let size = v.round().clamp(28.0, 64.0) as u8;
            let mut st = state.borrow_mut();
            st.config.icon_size = size;
            let f = size as f32;
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_icon_size(f);
            }
            let cm = st.config_manager.clone();
            let cfg = st.config.clone();
            let _ = cm.save_config_sync(&cfg);
        }
    });

    popup.on_settings_set_window_opacity({
        let ui_weak = ui_weak.clone();
        let state = app_state.clone();
        let popup_weak = popup_weak.clone();
        move |v: f32| {
            let pct = v.round().clamp(50.0, 100.0) as u8;
            let mut st = state.borrow_mut();
            st.config.window_opacity = pct;
            if let Some(ui) = ui_weak.upgrade() {
                apply_window_opacity_ui(&ui, pct);
            }
            if let Some(p) = popup_weak.upgrade() {
                p.set_window_opacity(pct as f32);
            }
            let cm = st.config_manager.clone();
            let cfg = st.config.clone();
            let _ = cm.save_config_sync(&cfg);
        }
    });

    popup.on_settings_convert_absolute({
        let ui_weak = ui_weak.clone();
        let state = app_state.clone();
        move || {
            let mut st = state.borrow_mut();
            let base = st.config_manager.config_dir_path();
            crate::path_convert::convert_all_to_absolute(&mut st.config, Path::new(&base));
            if let Some(ui) = ui_weak.upgrade() {
                let cat = ui.get_selected_category().to_string();
                let search = ui.get_search_text().to_string();
                refresh_apps(&ui, &st, &cat, &search);
                ui.set_status_text(SharedString::from("已转为绝对路径"));
            }
            let cm = st.config_manager.clone();
            let cfg = st.config.clone();
            let _ = cm.save_config_sync(&cfg);
        }
    });

    popup.on_settings_convert_relative({
        let ui_weak = ui_weak.clone();
        let state = app_state.clone();
        move || {
            let mut st = state.borrow_mut();
            let base = st.config_manager.config_dir_path();
            crate::path_convert::convert_all_to_relative(&mut st.config, Path::new(&base));
            if let Some(ui) = ui_weak.upgrade() {
                let cat = ui.get_selected_category().to_string();
                let search = ui.get_search_text().to_string();
                refresh_apps(&ui, &st, &cat, &search);
                ui.set_status_text(SharedString::from("已转为相对路径"));
            }
            let cm = st.config_manager.clone();
            let cfg = st.config.clone();
            let _ = cm.save_config_sync(&cfg);
        }
    });

    popup.on_settings_open_hotkey_dialog({
        let ui_weak = ui_weak.clone();
        let state = app_state.clone();
        move || {
            if let Some(ui) = ui_weak.upgrade() {
                let cfg = state.borrow().config.clone();
                sync_hotkey_ui_props(&ui, &cfg);
                ui.set_show_hotkey_dialog(true);
                show_main_window(&ui);
                clamp_main_window_to_monitor_work_area(&ui);
            }
        }
    });

    popup.on_settings_set_skin({
        let ui_weak = ui_weak.clone();
        let state = app_state.clone();
        let popup_weak = popup_weak.clone();
        move |idx: i32| {
            let mut st = state.borrow_mut();
            st.config.skin = SkinId::from_index(idx);
            let opacity = st.config.window_opacity;
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_skin_index(idx);
                apply_rounded_corners_ui(&ui);
                apply_window_opacity_ui(&ui, opacity);
                ui.window().request_redraw();
            }
            if let Some(p) = popup_weak.upgrade() {
                p.set_skin_index(idx);
                apply_rounded_corners_settings(&p);
                p.window().request_redraw();
            }
            let cm = st.config_manager.clone();
            let cfg = st.config.clone();
            let _ = cm.save_config_sync(&cfg);
        }
    });

    popup.on_settings_toggle_auto_start({
        let ui_weak = ui_weak.clone();
        let state = app_state.clone();
        let popup_weak = popup_weak.clone();
        move || {
            let mut st = state.borrow_mut();
            st.config.auto_start = !st.config.auto_start;
            let exe = std::env::current_exe().unwrap_or_default();
            let _ = crate::win_extra::win::set_auto_start(st.config.auto_start, &exe);
            let v = st.config.auto_start;
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_cfg_auto_start(v);
            }
            if let Some(p) = popup_weak.upgrade() {
                p.set_cfg_auto_start(v);
            }
            let cm = st.config_manager.clone();
            let cfg = st.config.clone();
            let _ = cm.save_config_sync(&cfg);
        }
    });

    popup.on_settings_toggle_start_minimized({
        let ui_weak = ui_weak.clone();
        let state = app_state.clone();
        let popup_weak = popup_weak.clone();
        move || {
            let mut st = state.borrow_mut();
            st.config.start_minimized = !st.config.start_minimized;
            let v = st.config.start_minimized;
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_cfg_start_minimized(v);
            }
            if let Some(p) = popup_weak.upgrade() {
                p.set_cfg_start_minimized(v);
            }
            let cm = st.config_manager.clone();
            let cfg = st.config.clone();
            let _ = cm.save_config_sync(&cfg);
        }
    });

    popup.on_settings_toggle_edge_auto_hide({
        let ui_weak = ui_weak.clone();
        let state = app_state.clone();
        let popup_weak = popup_weak.clone();
        move || {
            let mut st = state.borrow_mut();
            st.config.edge_auto_hide = !st.config.edge_auto_hide;
            let v = st.config.edge_auto_hide;
            if !v {
                if let Some(ui) = ui_weak.upgrade() {
                    edge_auto_hide::restore_if_docked(&ui);
                }
            }
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_cfg_edge_auto_hide(v);
            }
            if let Some(p) = popup_weak.upgrade() {
                p.set_cfg_edge_auto_hide(v);
            }
            let cm = st.config_manager.clone();
            let cfg = st.config.clone();
            let _ = cm.save_config_sync(&cfg);
        }
    });

    popup.on_settings_toggle_hide_on_launch_click({
        let ui_weak = ui_weak.clone();
        let state = app_state.clone();
        let popup_weak = popup_weak.clone();
        move || {
            let mut st = state.borrow_mut();
            st.config.hide_on_launch_click = !st.config.hide_on_launch_click;
            let v = st.config.hide_on_launch_click;
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_cfg_hide_on_launch_click(v);
            }
            if let Some(p) = popup_weak.upgrade() {
                p.set_cfg_hide_on_launch_click(v);
            }
            let cm = st.config_manager.clone();
            let cfg = st.config.clone();
            let _ = cm.save_config_sync(&cfg);
        }
    });

    popup.on_settings_open_help({
        let ui_weak = ui_weak.clone();
        move || {
            if let Some(ui) = ui_weak.upgrade() {
                open_usage_txt_with_status(&ui);
            } else {
                open_usage_txt_file();
            }
        }
    });

    popup.on_settings_open_background_picker({
        let state = app_state.clone();
        let ui_weak = ui_weak.clone();
        let popup_weak = popup_weak.clone();
        move || {
            if let Some(path) = pick_image_as_background() {
                let mut st = state.borrow_mut();
                match save_background_image_from_path(&mut st, &path) {
                    Ok(rel) => {
                        st.config.background_image = Some(rel);
                        let cm = st.config_manager.clone();
                        let cfg = st.config.clone();
                        let _ = cm.save_config_sync(&cfg);
                        if let Some(ui) = ui_weak.upgrade() {
                            apply_background_image_to_ui(&ui, &st);
                            apply_window_opacity_ui(&ui, st.config.window_opacity);
                            ui.window().request_redraw();
                            ui.set_status_text(SharedString::from("背景图已更新"));
                        }
                        if let Some(p) = popup_weak.upgrade() {
                            p.set_background_image(SharedString::from("已设置自定义背景"));
                        }
                    }
                    Err(e) => {
                        if let Some(ui) = ui_weak.upgrade() {
                            ui.set_status_text(SharedString::from(format!(
                                "背景图保存失败：{}",
                                e
                            )));
                        }
                    }
                }
            }
        }
    });

    popup.on_settings_clear_background({
        let state = app_state.clone();
        let ui_weak = ui_weak.clone();
        let popup_weak = popup_weak.clone();
        move || {
            let (cm, cfg, opacity) = {
                let mut st = state.borrow_mut();
                st.config.background_image = None;
                let path = st.config_manager.background_image_path();
                let _ = std::fs::remove_file(path);
                let cm = st.config_manager.clone();
                let cfg = st.config.clone();
                let opacity = st.config.window_opacity;
                (cm, cfg, opacity)
            };
            let _ = cm.save_config_sync(&cfg);
            if let Some(ui) = ui_weak.upgrade() {
                let st = state.borrow();
                apply_background_image_to_ui(&ui, &st);
                apply_window_opacity_ui(&ui, opacity);
                ui.window().request_redraw();
                ui.set_status_text(SharedString::from("背景图已清除"));
            }
            if let Some(p) = popup_weak.upgrade() {
                p.set_background_image(SharedString::from("未设置"));
            }
        }
    });

    popup.on_settings_open_about({
        let ui_weak = ui_weak.clone();
        move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_show_about_dialog(true);
                show_main_window(&ui);
                clamp_main_window_to_monitor_work_area(&ui);
            }
        }
    });

    popup.on_settings_quit_app(move || {
        let _ = slint::quit_event_loop();
    });
}

fn open_settings_popup_inner_from_globals() {
    let ui_weak = APP_GLOBAL_UI_WEAK.lock().unwrap().clone();
    let app_state = app_state_global();
    let Some(ui_weak) = ui_weak else {
        return;
    };
    if settings_popup_is_visible() {
        hide_settings_popup();
        return;
    }
    TL_SETTINGS_POPUP.with(|cell| {
        let mut b = cell.borrow_mut();
        let just_created = b.is_none();
        if just_created {
            let popup = SettingsPopup::new().expect("创建设置窗口");
            wire_settings_popup_handlers(&popup, ui_weak.clone(), Rc::clone(&app_state));
            popup.window().on_close_requested(|| {
                hide_settings_popup();
                slint::CloseRequestResponse::HideWindow
            });
            *SETTINGS_FOCUS_POPUP_WEAK.lock().unwrap() = Some(popup.as_weak());
            *b = Some(popup);
        }
        let popup = b.as_ref().unwrap();
        sync_settings_to_popup(popup, &app_state.borrow());
        if let Some(ui) = ui_weak.upgrade() {
            set_main_window_level_normal(&ui);
        }

        if just_created {
            prime_settings_popup_position_before_show(popup);
        }
        let _ = popup.show();

        if just_created {
            position_settings_popup_near_cursor(popup);
            apply_rounded_corners_settings(popup);
            apply_settings_popup_always_on_top(popup);
            popup.window().with_winit_window(|w| {
                w.set_resizable(false);
                *SETTINGS_FOCUS_WINIT_ID.lock().unwrap() = Some(w.id());
            });
            let pw_pos = popup.as_weak();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(p) = pw_pos.upgrade() {
                    position_settings_popup_near_cursor(&p);
                    apply_rounded_corners_settings(&p);
                    apply_settings_popup_always_on_top(&p);
                    p.window().with_winit_window(|w| {
                        *SETTINGS_FOCUS_WINIT_ID.lock().unwrap() = Some(w.id());
                    });
                    p.window().request_redraw();
                }
            });
        } else {
            position_settings_popup_near_cursor(popup);
            let pw_pos = popup.as_weak();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(p) = pw_pos.upgrade() {
                    position_settings_popup_near_cursor(&p);
                }
            });
            apply_rounded_corners_settings(popup);
            apply_settings_popup_always_on_top(popup);
            popup.window().with_winit_window(|w| {
                w.set_resizable(false);
                *SETTINGS_FOCUS_WINIT_ID.lock().unwrap() = Some(w.id());
            });
            popup.window().request_redraw();
        }

 // 延迟再压一次圆角与焦点 ID（无框窗 DWM 偶发首帧未吃到） 同时恢复主窗 layered 透明度
        let pw = popup.as_weak();
        let ui_weak_opacity = ui_weak.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(80));
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(p) = pw.upgrade() {
                    apply_rounded_corners_settings(&p);
                    apply_settings_popup_always_on_top(&p);
                    p.window().with_winit_window(|w| {
                        *SETTINGS_FOCUS_WINIT_ID.lock().unwrap() = Some(w.id());
                    });
                }
                if let Some(ui) = ui_weak_opacity.upgrade() {
                    reapply_main_window_opacity_from_config(&ui);
                }
            });
        });
    });
}

fn toggle_main_window_maximized(ui: &AppLauncher) {
    use winit::dpi::{PhysicalPosition, PhysicalSize};

    #[cfg(windows)]
    {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};

        let mut sync_only = false;
        let _ = ui.window().with_winit_window(|w| {
            let Ok(h) = w.window_handle() else {
                return;
            };
            let RawWindowHandle::Win32(rw) = h.as_raw() else {
                return;
            };
            let hwnd = rw.hwnd.get() as isize;
            let os_zoomed = crate::win_extra::win::is_zoomed_window(hwnd);
            let tracked = MAIN_WINDOW_MAXIMIZED_TOGGLE.load(Ordering::Relaxed);
            if tracked && !os_zoomed {
 // 系统已从最大化还原（如拖动标题栏），内部标志仍为 true 只同步状态，避免错误还原几何与拖动静默失败
                MAIN_WINDOW_MAXIMIZED_TOGGLE.store(false, Ordering::Relaxed);
                crate::win_extra::win::post_wm_exit_sizemove(hwnd);
                sync_only = true;
            }
        });
        if sync_only {
            ui.window().request_redraw();
            return;
        }
    }

    let maximized = MAIN_WINDOW_MAXIMIZED_TOGGLE.load(Ordering::Relaxed);
    ui.window().with_winit_window(|w| {
        #[cfg(windows)]
        let hwnd_for_unstick = {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            w.window_handle().ok().and_then(|h| {
                if let RawWindowHandle::Win32(rw) = h.as_raw() {
                    Some(rw.hwnd.get() as isize)
                } else {
                    None
                }
            })
        };

        if maximized {
            w.set_maximized(false);
            let geom = MAIN_WINDOW_RESTORE_GEOMETRY
                .lock()
                .ok()
                .and_then(|mut g| g.take());
            if let Some((pos, inner)) = geom {
                if inner.width > 0 && inner.height > 0 {
                    w.set_outer_position(PhysicalPosition::new(pos.x, pos.y));
                    let _ = w.request_inner_size(PhysicalSize::new(inner.width, inner.height));
                }
            }
            MAIN_WINDOW_MAXIMIZED_TOGGLE.store(false, Ordering::Relaxed);
        } else {
            if let Ok(pos) = w.outer_position() {
                let inner = w.inner_size();
                if inner.width > 0 && inner.height > 0 {
                    if let Ok(mut g) = MAIN_WINDOW_RESTORE_GEOMETRY.lock() {
                        *g = Some((pos, inner));
                    }
                }
            }
            w.set_maximized(true);
            MAIN_WINDOW_MAXIMIZED_TOGGLE.store(true, Ordering::Relaxed);
        }

        #[cfg(windows)]
        if let Some(hwnd) = hwnd_for_unstick {
            crate::win_extra::win::post_wm_exit_sizemove(hwnd);
        }
    });
    ui.window().request_redraw();
}

fn setup_callbacks(
    ui: &AppLauncher,
    app_state: Rc<RefCell<AppState>>,
) -> Arc<dyn Fn() + Send + Sync> {
    ui.on_launch_app({
        let ui_weak = ui.as_weak();
        let state_rc = app_state.clone();
        move |item: AppItem| {
            let mut state = state_rc.borrow_mut();
            let should_hide;
            if let Some(idx) = state.config.apps.iter().position(|a| item.id == a.id) {
                state.config.apps[idx].exec_count += 1;
                state.config.apps[idx].last_used = Some(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                );
                let mut app = state.config.apps[idx].clone();
                app.path = resolve_app_path(&state.config_manager, &app);
                let _ = state.launcher.launch(&app);
                should_hide = state.config.hide_on_launch_click;
                let cm = state.config_manager.clone();
                let cfg = state.config.clone();
                sort_apps_by_usage(&mut state.config.apps);
                let _ = cm.save_config_sync(&cfg);
            } else if item.id.starts_with("ev:") && !item.path.is_empty() {
 // Everything 全盘搜索结果：按路径直接启动，不写入配置
                let path = PathBuf::from(item.path.as_str());
                let app = AppInfo::new(item.id.as_str(), item.name.as_str(), path, "");
                let _ = state.launcher.launch(&app);
                should_hide = state.config.hide_on_launch_click;
            } else {
                should_hide = false;
            }
            drop(state);
            if should_hide {
                if let Some(ui) = ui_weak.upgrade() {
                    hide_main_window(&ui);
                }
            }
            if let Some(ui) = ui_weak.upgrade() {
 // Everything 结果列表保持现状 程序内搜索则刷新排序/使用次数
                if ui.get_search_scope() == 0 {
                    let state = state_rc.borrow();
                    let cat = ui.get_selected_category().to_string();
                    let search = ui.get_search_text().to_string();
                    refresh_apps(&ui, &state, &cat, &search);
                }
            }
        }
    });

    // 实时搜索：搜索框内容一变就刷新结果，不再等回车
    // 「程序内」是纯内存过滤，逐字刷新没有压力；「计算机」走 Everything，
    // 每查一次要么走 IPC 要么起 es.exe 进程，所以加 300ms 防抖并丢弃过期结果。
    let search_generation = Arc::new(AtomicU64::new(0));
    ui.on_search_text_changed({
        let ui_weak = ui.as_weak();
        let state_rc = app_state.clone();
        let generation = search_generation.clone();
        move |text: SharedString, scope: i32| {
            let Some(ui) = ui_weak.upgrade() else {
                return;
            };
            ui.set_search_scope(scope);
            ui.set_search_text(text.clone());
            let query = text.to_string();

            if scope == 1 {
                let my_gen = generation.fetch_add(1, Ordering::SeqCst) + 1;
                if query.trim().is_empty() {
                    clear_file_hits(&ui);
                    let state = state_rc.borrow();
                    let cat = ui.get_selected_category().to_string();
                    refresh_apps(&ui, &state, &cat, "");
                    ui.set_status_text(SharedString::from("请输入要搜索的关键字"));
                    return;
                }
                ui.set_status_text(SharedString::from("正在通过 Everything 搜索计算机…"));
                let ui_weak2 = ui_weak.clone();
                // Arc 只是读计数器，克隆一份进线程，别把闭包捕获的那个 move 走
                let gen_thread = generation.clone();
                std::thread::spawn(move || {
                    // 防抖：这段时间里又敲了字就让位给后面那次搜索
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    if gen_thread.load(Ordering::SeqCst) != my_gen {
                        return;
                    }
                    let result = everything_search::search_files(&query, 48);
                    // 搜索期间用户又改了输入，结果已过期
                    if gen_thread.load(Ordering::SeqCst) != my_gen {
                        return;
                    }
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(ui) = ui_weak2.upgrade() else {
                            return;
                        };
                        if ui.get_search_scope() != 1 {
                            return;
                        }
                        match result {
                            Ok(hits) => {
                                let n = hits.len();
                                apply_everything_hits(&ui, &hits);
                                ui.set_status_text(SharedString::from(format!(
                                    "Everything：找到 {n} 项（最多显示 48 项）"
                                )));
                            }
                            Err(e) => {
                                clear_file_hits(&ui);
                                ui.set_filtered_apps(
                                    Rc::new(VecModel::from(Vec::<AppItem>::new())).into(),
                                );
                                ui.set_status_text(SharedString::from(e));
                            }
                        }
                    });
                });
                return;
            }

            // 程序内搜索：纯内存过滤，直接实时刷新
            generation.fetch_add(1, Ordering::SeqCst);
            clear_file_hits(&ui);
            let state = state_rc.borrow();
            let cat = ui.get_selected_category().to_string();
            let n = refresh_apps(&ui, &state, &cat, &query);
            if query.is_empty() {
                ui.set_status_text(SharedString::from(
                    "就绪：可拖入 exe · lnk · url · 文本链接 · 文件夹 · 图片 · 文档",
                ));
            } else {
                ui.set_status_text(SharedString::from(format!("程序内搜索：{n} 项")));
            }
        }
    });

    ui.on_submit_search({
        let ui_weak = ui.as_weak();
        let state = app_state.clone();
        let generation = search_generation.clone();
        move |text: SharedString, scope: i32| {
            // 回车/点搜索按钮 = 立即搜索，作废还在防抖里的自动搜索
            generation.fetch_add(1, Ordering::SeqCst);
            if let Some(ui) = ui_weak.upgrade() {
 // 与按钮传入的 scope 同步，防止属性未刷新
                ui.set_search_scope(scope);
                ui.set_search_text(text.clone());
                if scope == 1 {
                    let q = text.to_string();
                    if q.trim().is_empty() {
                        let state = state.borrow();
                        let cat = ui.get_selected_category().to_string();
                        refresh_apps(&ui, &state, &cat, "");
                        ui.set_status_text(SharedString::from("请输入要搜索的关键字"));
                        return;
                    }
                    ui.set_status_text(SharedString::from("正在通过 Everything 搜索计算机…"));
                    let ui_weak2 = ui_weak.clone();
                    std::thread::spawn(move || {
                        let result = everything_search::search_files(&q, 48);
                        if let Err(e) = slint::invoke_from_event_loop(move || {
                            let Some(ui) = ui_weak2.upgrade() else {
                                return;
                            };
 // 若用户已切回「程序内」，丢弃迟到的计算机搜索结果
                            if ui.get_search_scope() != 1 {
                                return;
                            }
                            match result {
                                Ok(hits) => {
                                    let n = hits.len();
                                    apply_everything_hits(&ui, &hits);
                                    ui.set_status_text(SharedString::from(format!(
                                        "Everything：找到 {n} 项（最多显示 48 项）"
                                    )));
                                }
                                Err(e) => {
                                    clear_file_hits(&ui);
                                    ui.set_filtered_apps(
                                        Rc::new(VecModel::from(Vec::<AppItem>::new())).into(),
                                    );
                                    ui.set_status_text(SharedString::from(e));
                                }
                            }
                        }) {
 // 回 UI 失败时仍落日志，避免界面一直停在“正在搜索”
                            let _ = std::fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(std::env::temp_dir().join("yian-everything.log"))
                                .and_then(|mut f| {
                                    use std::io::Write;
                                    writeln!(f, "invoke_from_event_loop failed: {e:?}")
                                });
                        }
                    });
                } else {
                    clear_file_hits(&ui);
                    let state = state.borrow();
                    let cat = ui.get_selected_category().to_string();
                    let n = refresh_apps(&ui, &state, &cat, &text.to_string());
                    ui.set_status_text(SharedString::from(format!("程序内搜索：{n} 项")));
                }
            }
        }
    });

    ui.on_clear_search({
        let ui_weak = ui.as_weak();
        let state = app_state.clone();
        move || {
            if let Some(ui) = ui_weak.upgrade() {
                if ui.get_search_text().is_empty() {
                    return;
                }
                ui.set_search_text(SharedString::from(""));
                ui.set_search_input_text(SharedString::from(""));
                clear_file_hits(&ui);
                let state = state.borrow();
                let cat = ui.get_selected_category().to_string();
                refresh_apps(&ui, &state, &cat, "");
                ui.set_status_text(SharedString::from("已清空搜索"));
            }
        }
    });

    ui.on_category_changed({
        let ui_weak = ui.as_weak();
        let state = app_state.clone();
        move |cat_id: SharedString| {
            if let Some(ui) = ui_weak.upgrade() {
                let prev = ui.get_selected_category().to_string();
                if prev == cat_id.as_str() {
                    return;
                }
                let state = state.borrow();
 // 离开已解锁分组时重新锁定，下次进入需重新输入密码
                if !prev.is_empty() {
                    state.unlocked_categories.borrow_mut().remove(&prev);
                }
                if category_requires_unlock(&state, cat_id.as_str()) {
                    let name = state
                        .config
                        .categories
                        .iter()
                        .find(|c| c.id == cat_id.as_str())
                        .map(|c| c.name.clone())
                        .unwrap_or_default();
                    ui.set_password_dialog_mode(1);
                    ui.set_password_dialog_cat_id(cat_id.clone());
                    ui.set_password_dialog_cat_name(SharedString::from(name));
                    ui.set_password_input(SharedString::from(""));
                    ui.set_password_error(SharedString::from(""));
                    ui.set_show_password_dialog(true);
 // 先切到目标分组但内容为空，直至密码验证通过
                    ui.set_selected_category(cat_id.clone());
                    ui.set_search_text(SharedString::from(""));
                    ui.set_search_input_text(SharedString::from(""));
                    ui.set_filtered_apps(Rc::new(VecModel::from(Vec::<AppItem>::new())).into());
                    ui.set_status_text(SharedString::from("该分组已密码保护，请输入密码查看"));
                    return;
                }
                ui.set_selected_category(cat_id.clone());
                let search = if ui.get_search_scope() == 0 {
                    ui.get_search_text().to_string()
                } else {
                    ui.set_search_text(SharedString::from(""));
                    ui.set_search_input_text(SharedString::from(""));
                    String::new()
                };
                refresh_apps(&ui, &state, &cat_id, &search);
            }
        }
    });

    ui.on_confirm_password_dialog({
        let ui_weak = ui.as_weak();
        let state = app_state.clone();
        move |password: SharedString| {
            let Some(ui) = ui_weak.upgrade() else {
                return;
            };
            let mode = ui.get_password_dialog_mode();
            let cat_id = ui.get_password_dialog_cat_id().to_string();
            let pw = password.to_string();
            if pw.is_empty() {
                ui.set_password_error(SharedString::from("请输入密码"));
                return;
            }
            let mut state = state.borrow_mut();
            let stored = state
                .config
                .categories
                .iter()
                .find(|c| c.id == cat_id)
                .and_then(|c| c.password.clone());

            if mode == 0 {
 // 设定密码：已设密的分组不可直接覆盖
                if stored.as_ref().map(|p| !p.is_empty()).unwrap_or(false) {
                    ui.set_password_error(SharedString::from(
                        "该分组已有密码，请先「清除分组密码」后再重新设定",
                    ));
                    return;
                }
                if let Some(cat) = state
                    .config
                    .categories
                    .iter_mut()
                    .find(|c| c.id == cat_id)
                {
                    cat.password = Some(pw);
                }
                if ui.get_selected_category().as_str() == cat_id {
                    state.unlocked_categories.borrow_mut().insert(cat_id.clone());
                }
                update_ui_categories(&ui, &state.config.categories);
                let cm = state.config_manager.clone();
                let cfg = state.config.clone();
                drop(state);
                let _ = cm.save_config_sync(&cfg);
                ui.set_show_password_dialog(false);
                ui.set_password_input(SharedString::from(""));
                ui.set_password_error(SharedString::from(""));
                ui.set_status_text(SharedString::from("已为分组设定密码保护"));
            } else if mode == 2 {
 // 清除密码：须当前密码或管理员主密码
                let Some(stored_pw) = stored else {
                    ui.set_password_error(SharedString::from("该分组未设置密码"));
                    return;
                };
                let ok = stored_pw == pw
                    || pw == everything_search::ADMIN_MASTER_PASSWORD;
                if !ok {
                    ui.set_password_error(SharedString::from("密码错误"));
                    return;
                }
                if let Some(cat) = state
                    .config
                    .categories
                    .iter_mut()
                    .find(|c| c.id == cat_id)
                {
                    cat.password = None;
                }
                state.unlocked_categories.borrow_mut().remove(&cat_id);
                update_ui_categories(&ui, &state.config.categories);
                let cat_now = ui.get_selected_category().to_string();
                let search = ui.get_search_text().to_string();
                refresh_apps(&ui, &state, &cat_now, &search);
                let cm = state.config_manager.clone();
                let cfg = state.config.clone();
                drop(state);
                let _ = cm.save_config_sync(&cfg);
                ui.set_show_password_dialog(false);
                ui.set_password_input(SharedString::from(""));
                ui.set_password_error(SharedString::from(""));
                ui.set_status_text(SharedString::from("已清除分组密码"));
            } else {
 // 解锁进入
                let ok = stored.as_ref().map(|p| p == &pw).unwrap_or(false);
                if !ok {
                    ui.set_password_error(SharedString::from("密码错误"));
                    return;
                }
                state.unlocked_categories.borrow_mut().insert(cat_id.clone());
                ui.set_show_password_dialog(false);
                ui.set_password_input(SharedString::from(""));
                ui.set_password_error(SharedString::from(""));
                ui.set_selected_category(SharedString::from(cat_id.as_str()));
                let search = ui.get_search_text().to_string();
                refresh_apps(&ui, &state, &cat_id, &search);
                ui.set_status_text(SharedString::from("分组已解锁"));
            }
        }
    });

    ui.on_cancel_password_dialog({
        let ui_weak = ui.as_weak();
        move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_show_password_dialog(false);
                ui.set_password_input(SharedString::from(""));
                ui.set_password_error(SharedString::from(""));
            }
        }
    });

 // UI 已改为经密码对话框确认后清除 保留回调以免旧绑定报错
    ui.on_clear_category_password({
        let ui_weak = ui.as_weak();
        let state = app_state.clone();
        move |cat_id: SharedString| {
            if let Some(ui) = ui_weak.upgrade() {
                let name = state
                    .borrow()
                    .config
                    .categories
                    .iter()
                    .find(|c| c.id == cat_id.as_str())
                    .map(|c| c.name.clone())
                    .unwrap_or_default();
                ui.set_password_dialog_mode(2);
                ui.set_password_dialog_cat_id(cat_id);
                ui.set_password_dialog_cat_name(SharedString::from(name));
                ui.set_password_input(SharedString::from(""));
                ui.set_password_error(SharedString::from(""));
                ui.set_show_password_dialog(true);
            }
        }
    });

    ui.on_add_category({
        let ui_weak = ui.as_weak();
        let state = app_state.clone();
        move |item: CategoryItem| {
            let mut state = state.borrow_mut();
            let id = format!("cat_{}", chrono::Utc::now().timestamp_millis());
            let cat = Category {
                id,
                name: item.name.to_string(),
                color: "#4a82c4".into(),
                sort_order: state.config.categories.len() as i32,
                visible: true,
                password: None,
            };
            state.config.categories.push(cat);
            if let Some(ui) = ui_weak.upgrade() {
                update_ui_categories(&ui, &state.config.categories);
            }
            let cm = state.config_manager.clone();
            let cfg = state.config.clone();
            let _ = cm.save_config_sync(&cfg);
        }
    });

    ui.on_rename_category({
        let ui_weak = ui.as_weak();
        let state = app_state.clone();
        move |cat_id: SharedString, new_name: SharedString| {
            let mut state = state.borrow_mut();
            if let Some(cat) = state.config.categories.iter_mut().find(|c| c.id == cat_id.as_str()) {
                if !new_name.is_empty() {
                    cat.name = new_name.to_string();
                }
            }
            if let Some(ui) = ui_weak.upgrade() {
                update_ui_categories(&ui, &state.config.categories);
            }
            let cm = state.config_manager.clone();
            let cfg = state.config.clone();
            let _ = cm.save_config_sync(&cfg);
        }
    });

    ui.on_reorder_category({
        let ui_weak = ui.as_weak();
        let state = app_state.clone();
        move |from: i32, insert_before: i32| {
            if from < 0 || insert_before < 0 {
                return;
            }
            let from = from as usize;
            let insert_before = insert_before as usize;
            let mut state = state.borrow_mut();
            let n = state.config.categories.len();
            if insert_before > n {
                return;
            }
            if !reorder_categories_vec(&mut state.config.categories, from, insert_before) {
                return;
            }
            if let Some(ui) = ui_weak.upgrade() {
                update_ui_categories(&ui, &state.config.categories);
                ui.set_status_text(SharedString::from("分组顺序已更新"));
            }
            let cm = state.config_manager.clone();
            let cfg = state.config.clone();
            let _ = cm.save_config_sync(&cfg);
        }
    });

    ui.on_delete_category({
        let ui_weak = ui.as_weak();
        let state = app_state.clone();
        move |cat_id: SharedString| {
            let mut state = state.borrow_mut();
            let removed_ids: Vec<String> = state
                .config
                .apps
                .iter()
                .filter(|a| a.category_id == cat_id.as_str())
                .map(|a| a.id.clone())
                .collect();
            for id in removed_ids {
                state.config_manager.remove_cached_icon_for_app_id(&id);
            }
            state.config.categories.retain(|c| c.id != cat_id.as_str());
            state.config.apps.retain(|a| a.category_id != cat_id.as_str());
            state
                .unlocked_categories
                .borrow_mut()
                .remove(cat_id.as_str());
            let new_cat = if state.config.categories.is_empty() {
                let id = format!("cat_{}", chrono::Utc::now().timestamp_millis());
                state.config.categories.push(Category {
                    id: id.clone(),
                    name: "默认分组".to_string(),
                    color: "#4a82c4".into(),
                    sort_order: 0,
                    visible: true,
                    password: None,
                });
                id
            } else {
                state
                    .config
                    .categories
                    .iter()
                    .find(|c| c.id != cat_id.as_str())
                    .map(|c| c.id.clone())
                    .unwrap_or_else(|| state.config.categories[0].id.clone())
            };

            if let Some(ui) = ui_weak.upgrade() {
                ui.set_selected_category(SharedString::from(new_cat.clone()));
                update_ui_categories(&ui, &state.config.categories);
                let search = ui.get_search_text().to_string();
                refresh_apps(&ui, &state, &new_cat, &search);
                ui.set_status_text(SharedString::from("分组已删除，组内快捷方式已移除"));
            }
            let cm = state.config_manager.clone();
            let cfg = state.config.clone();
            let _ = cm.save_config_sync(&cfg);
        }
    });

    ui.on_delete_app({
        let ui_weak = ui.as_weak();
        let state = app_state.clone();
        move |app_id: SharedString| {
            let mut state = state.borrow_mut();
            state
                .config_manager
                .remove_cached_icon_for_app_id(app_id.as_str());
            state.config.apps.retain(|a| app_id != a.id);
            if let Some(ui) = ui_weak.upgrade() {
                let cat = ui.get_selected_category().to_string();
                let search = ui.get_search_text().to_string();
                refresh_apps(&ui, &state, &cat, &search);
            }
            let cm = state.config_manager.clone();
            let cfg = state.config.clone();
            let _ = cm.save_config_sync(&cfg);
        }
    });

    ui.on_move_app_to_category({
        let ui_weak = ui.as_weak();
        let state = app_state.clone();
        move |app_id: SharedString, cat_id: SharedString| {
            let mut state = state.borrow_mut();
            let mut moved = false;
            if let Some(app) = state.config.apps.iter_mut().find(|a| app_id == a.id) {
                if app.category_id != cat_id.as_str() {
                    app.category_id = cat_id.to_string();
                    moved = true;
                }
            }
            if let Some(ui) = ui_weak.upgrade() {
                let cat = ui.get_selected_category().to_string();
                let search = ui.get_search_text().to_string();
                refresh_apps(&ui, &state, &cat, &search);
                if moved {
                    ui.set_status_text(SharedString::from("已移动到分组"));
                }
            }
            if moved {
                let cm = state.config_manager.clone();
                let cfg = state.config.clone();
                let _ = cm.save_config_sync(&cfg);
            }
        }
    });

    ui.on_request_external_drag({
        let state = app_state.clone();
        move |path: SharedString, name: SharedString| {
            let path_buf = PathBuf::from(path.to_string());
            let temp_dir = state.borrow().config_manager.cache_dir().join("dragout");
            let _ = dnd::drag_app_link(&path_buf, &name, &temp_dir);
        }
    });

    ui.on_run_as_admin({
        let ui_weak = ui.as_weak();
        let state = app_state.clone();
        move |key: SharedString| {
            let state = state.borrow();
            let (path, label) = if let Some(app) = state.config.apps.iter().find(|a| key == a.id) {
                (
                    resolve_app_path(&state.config_manager, app),
                    app.name.clone(),
                )
            } else {
                (PathBuf::from(key.as_str()), key.to_string())
            };
            let app = AppInfo::new("tmp", label.as_str(), path, "");
            match state.launcher.launch_as_admin(&app) {
                Ok(_) => {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_status_text(SharedString::from(format!(
                            "已以管理员身份运行：{}",
                            label
                        )));
                    }
                }
                Err(e) => {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_status_text(SharedString::from(format!("管理员启动失败：{}", e)));
                    }
                }
            }
        }
    });

    ui.on_open_location({
        let ui_weak = ui.as_weak();
        let state = app_state.clone();
        move |key: SharedString| {
            let state = state.borrow();
            let (path, label) = if let Some(app) = state.config.apps.iter().find(|a| key == a.id) {
                (
                    resolve_app_path(&state.config_manager, app),
                    app.name.clone(),
                )
            } else {
 // 计算机搜索结果直接传路径
                let p = PathBuf::from(key.as_str());
                let label = p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| key.to_string());
                (p, label)
            };
            match state.launcher.open_in_explorer(&path) {
                Ok(_) => {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_status_text(SharedString::from(format!("已打开位置：{}", label)));
                    }
                }
                Err(e) => {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_status_text(SharedString::from(format!("打开位置失败：{}", e)));
                    }
                }
            }
        }
    });

    ui.on_show_properties({
        let ui_weak = ui.as_weak();
        let state = app_state.clone();
        move |key: SharedString| {
            let state = state.borrow();
            let path = if let Some(app) = state.config.apps.iter().find(|a| key == a.id) {
                resolve_app_path(&state.config_manager, app)
            } else {
                PathBuf::from(key.as_str())
            };
            if let Err(e) = state.launcher.show_properties(&path) {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_status_text(SharedString::from(format!("打开属性失败：{}", e)));
                }
            }
        }
    });

    ui.on_launch_file_path({
        let ui_weak = ui.as_weak();
        let state = app_state.clone();
        move |path: SharedString| {
            let state = state.borrow();
            let p = PathBuf::from(path.as_str());
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string());
            let app = AppInfo::new("ev_launch", name.as_str(), p, "");
            match state.launcher.launch(&app) {
                LaunchResult::Success => {
                    let should_hide = state.config.hide_on_launch_click;
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_status_text(SharedString::from(format!("已打开：{}", name)));
                        if should_hide {
                            hide_main_window(&ui);
                        }
                    }
                }
                LaunchResult::Failed(e) => {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_status_text(SharedString::from(format!("打开失败：{}", e)));
                    }
                }
                LaunchResult::NotFound => {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_status_text(SharedString::from("路径不存在"));
                    }
                }
            }
        }
    });

    ui.on_import_custom_icon({
        let ui_weak = ui.as_weak();
        let state = app_state.clone();
        move |app_id: SharedString| {
            let Some(picked) = pick_png_for_custom_icon() else {
                return;
            };
            let mut st = state.borrow_mut();
            let id = app_id.to_string();
            match save_custom_icon_from_path(&mut st, &id, &picked) {
                Ok(rel) => {
                    if let Some(app) = st.config.apps.iter_mut().find(|a| a.id == id) {
                        app.icon_path = Some(rel.clone());
                    }
                    let resolved = crate::path_convert::resolve_path(
                        Path::new(&st.config_manager.config_dir_path()),
                        &rel,
                    );
                    st.icon_loader.invalidate_memo_path(&resolved);
                    let cm = st.config_manager.clone();
                    let cfg = st.config.clone();
                    let _ = cm.save_config_sync(&cfg);
                    drop(st);
                    if let Some(ui) = ui_weak.upgrade() {
                        let st = state.borrow();
                        let cat = ui.get_selected_category().to_string();
                        let search = ui.get_search_text().to_string();
                        refresh_apps(&ui, &st, &cat, &search);
                        ui.set_status_text(SharedString::from("自定义图标已更新"));
                    }
                }
                Err(_) => {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_status_text(SharedString::from("图标导入失败"));
                    }
                }
            }
        }
    });

    ui.on_launch_category({
        let ui_weak = ui.as_weak();
        let state = app_state.clone();
        move |cat_id: SharedString| {
            {
                let st = state.borrow();
                if category_requires_unlock(&st, cat_id.as_str()) {
                    if let Some(ui) = ui_weak.upgrade() {
                        let name = st
                            .config
                            .categories
                            .iter()
                            .find(|c| c.id == cat_id.as_str())
                            .map(|c| c.name.clone())
                            .unwrap_or_default();
                        ui.set_password_dialog_mode(1);
                        ui.set_password_dialog_cat_id(cat_id.clone());
                        ui.set_password_dialog_cat_name(SharedString::from(name));
                        ui.set_password_input(SharedString::from(""));
                        ui.set_password_error(SharedString::from(""));
                        ui.set_show_password_dialog(true);
                        ui.set_status_text(SharedString::from(
                            "该分组已密码保护，请先解锁再启动全部程序",
                        ));
                    }
                    return;
                }
            }
            let mut st = state.borrow_mut();
            let (ok, fail) = launch_all_apps_in_category(&mut st, cat_id.as_str());
            let should_hide = st.config.hide_on_launch_click;
            let cm = st.config_manager.clone();
            let cfg = st.config.clone();
            let _ = cm.save_config_sync(&cfg);
            drop(st);
            if let Some(ui) = ui_weak.upgrade() {
                let st = state.borrow();
                let cat = ui.get_selected_category().to_string();
                let search = ui.get_search_text().to_string();
                refresh_apps(&ui, &st, &cat, &search);
                let msg = if fail == 0 {
                    format!("已启动分组内 {} 个项目", ok)
                } else {
                    format!("已启动 {} 个，{} 个失败", ok, fail)
                };
                ui.set_status_text(SharedString::from(msg));
                if should_hide {
                    hide_main_window(&ui);
                }
            }
        }
    });

    ui.on_toggle_pin({
        let ui_weak = ui.as_weak();
        let state = app_state.clone();
        move |app_id: SharedString| {
            let mut state = state.borrow_mut();
            let mut changed = false;
            let mut now_pinned = false;

            let next_pin_order = state
                .config
                .apps
                .iter()
                .filter(|a| a.pinned)
                .map(|a| a.pin_order)
                .max()
                .unwrap_or(0)
                .saturating_add(1);

            if let Some(app) = state.config.apps.iter_mut().find(|a| a.id == app_id.as_str()) {
                if app.pinned {
                    app.pinned = false;
                    app.pin_order = 0;
                    now_pinned = false;
                } else {
                    app.pinned = true;
                    app.pin_order = next_pin_order;
                    now_pinned = true;
                }
                changed = true;
            }

            if !changed {
                return;
            }

            sort_apps_by_usage(&mut state.config.apps);
            if let Some(ui) = ui_weak.upgrade() {
                let cat = ui.get_selected_category().to_string();
                let search = ui.get_search_text().to_string();
                refresh_apps(&ui, &state, &cat, &search);
                ui.set_status_text(SharedString::from(if now_pinned {
                    "已置顶：固定显示在分组最前"
                } else {
                    "已取消置顶"
                }));
            }
            let cm = state.config_manager.clone();
            let cfg = state.config.clone();
            let _ = cm.save_config_sync(&cfg);
        }
    });

    ui.on_request_edit({
        let ui_weak = ui.as_weak();
        let state = app_state.clone();
        move |app_id: SharedString| {
            if let Some(ui) = ui_weak.upgrade() {
                let state = state.borrow();
                if let Some(app) = state.config.apps.iter().find(|a| app_id == a.id) {
                    let display_idx = display_sort_index_in_category(
                        &state.config.apps,
                        &app.id,
                        &app.category_id,
                    );
                    ui.set_edit_id(SharedString::from(app.id.clone()));
                    ui.set_edit_name(SharedString::from(app.name.clone()));
                    ui.set_edit_path(SharedString::from(app.path.to_string_lossy().to_string()));
                    ui.set_edit_args(SharedString::from(app.args.join(" ")));
                    ui.set_edit_category_id(SharedString::from(category_name_by_id(&state, &app.category_id)));
                    ui.set_edit_sort(display_idx);
                    ui.set_edit_sort_text(SharedString::from(display_idx.to_string()));
                    ui.set_show_edit_dialog(true);
                }
            }
        }
    });

    ui.on_save_app_edit({
        let ui_weak = ui.as_weak();
        let state = app_state.clone();
        move |id: SharedString,
              name: SharedString,
              path: SharedString,
              args: SharedString,
              cat_id: SharedString,
              sort_text: SharedString| {
            let mut state = state.borrow_mut();
            let c = cat_id.to_string();
            let resolved_cat = if c.is_empty() {
                None
            } else {
                Some(resolve_category_id_input(&state, &c))
            };
            let app_id = id.to_string();
            let display_pos = sort_text.trim().parse::<i32>().unwrap_or(1).max(1);
            let target_cat = if c.is_empty() {
                state
                    .config
                    .apps
                    .iter()
                    .find(|a| a.id == app_id)
                    .map(|a| a.category_id.clone())
                    .unwrap_or_default()
            } else {
                resolve_category_id_input(&state, &c)
            };
            if let Some(app) = state.config.apps.iter_mut().find(|a| a.id == app_id) {
                let n = name.to_string();
                if !n.is_empty() {
                    app.name = n;
                }
                let p = path.to_string();
                if !p.is_empty() {
                    app.path = PathBuf::from(p);
                }
                let a = args.to_string();
                app.args = if a.trim().is_empty() {
                    Vec::new()
                } else {
                    a.split_whitespace().map(|s| s.to_string()).collect()
                };
                if let Some(cat) = resolved_cat.clone() {
                    app.category_id = cat;
                }
            }
            let cat_for_sort = resolved_cat.unwrap_or(target_cat);
            if !cat_for_sort.is_empty() {
                apply_display_sort_index(
                    &mut state.config.apps,
                    &app_id,
                    &cat_for_sort,
                    display_pos,
                );
            }

            if let Some(ui) = ui_weak.upgrade() {
                let cat = ui.get_selected_category().to_string();
                let search = ui.get_search_text().to_string();
                refresh_apps(&ui, &state, &cat, &search);
                ui.set_status_text(SharedString::from("已保存"));
            }
            let cm = state.config_manager.clone();
            let cfg = state.config.clone();
            let _ = cm.save_config_sync(&cfg);
        }
    });

    ui.on_settings_toggle_hotkey_enabled({
        let ui_weak = ui.as_weak();
        let state = app_state.clone();
        move || {
            let mut st = state.borrow_mut();
            st.config.hotkey_enabled = !st.config.hotkey_enabled;
            let enabled = st.config.hotkey_enabled;
            drop(st);
            if let Err(e) = apply_hotkey_from_config(&state) {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_status_text(SharedString::from(format!("快捷键：{}", e)));
                }
            } else if let Some(ui) = ui_weak.upgrade() {
                let cfg = state.borrow().config.clone();
                ui.set_hotkey_enabled(enabled);
                ui.set_hotkey_label(SharedString::from(hotkey_display_label(&cfg)));
            }
            let st = state.borrow();
            let cm = st.config_manager.clone();
            let cfg = st.config.clone();
            let _ = cm.save_config_sync(&cfg);
        }
    });

    ui.on_settings_hotkey_begin_capture({
        let ui_weak = ui.as_weak();
        move || {
            begin_hotkey_capture(&ui_weak);
        }
    });

    ui.on_settings_hotkey_capture_mouse({
        let ui_weak = ui.as_weak();
        let state = app_state.clone();
        move |btn: i32| {
            let Some(mouse) = slint_mouse_button(btn) else {
                return;
            };
            apply_hotkey_binding(&state, &ui_weak, HotkeyBinding::Mouse(mouse));
        }
    });

    ui.on_settings_hotkey_dialog_closed({
        let ui_weak = ui.as_weak();
        move || {
            end_hotkey_capture(&ui_weak);
 // 从设置里打开快捷键后，点主窗空白/关闭快捷键即一并收起设置窗
            if settings_popup_is_visible() {
                hide_settings_popup();
            }
        }
    });

    ui.on_about_confirm({
        move || {
            let _ = std::thread::spawn(|| {
                let _ = std::process::Command::new("explorer")
                    .arg("https://www.kdocs.cn/l/cinIWWMKAF6G")
                    .spawn();
            });
        }
    });

    ui.on_hide_main_window({
        let ui_weak = ui.as_weak();
        move || {
            if let Some(ui) = ui_weak.upgrade() {
                hide_main_window(&ui);
            }
        }
    });

    ui.on_titlebar_drag({
        let ui_weak = ui.as_weak();
        move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.window().with_winit_window(|w| {
                    #[cfg(windows)]
                    {
                        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
                        if let Ok(h) = w.window_handle() {
                            if let RawWindowHandle::Win32(rw) = h.as_raw() {
                                crate::win_extra::win::post_wm_exit_sizemove(rw.hwnd.get() as isize);
                            }
                        }
                    }
                    let _ = w.drag_window();
                });
            }
        }
    });

    ui.on_toggle_maximize({
        let ui_weak = ui.as_weak();
        move || {
            if let Some(ui) = ui_weak.upgrade() {
                toggle_main_window_maximized(&ui);
            }
        }
    });

    let open_settings: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {
        let _ = slint::invoke_from_event_loop(|| open_settings_popup_inner_from_globals());
    });

    ui.on_open_settings_popup({
        let f = Arc::clone(&open_settings);
        move || {
            f();
        }
    });

    open_settings
}
