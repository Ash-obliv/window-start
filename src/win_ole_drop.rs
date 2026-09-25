//! 主窗 OLE 拖放：文件走 CF_HDROP 文本也能接（记事本拖出来那种）
//! HTML Format / text/uri-list 也行 但只认开头一段里的 http(s) 见 extract_http_url_from_text

#![cfg(windows)]

#![allow(non_snake_case)]

use std::collections::HashSet;
use std::cell::RefCell;
use std::ffi::{c_void, OsString};
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::ptr;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use windows_sys::core::{w, IUnknown, GUID, HRESULT};
use windows_sys::Win32::Foundation::{
    BOOL, DRAGDROP_E_ALREADYREGISTERED, E_NOINTERFACE, E_POINTER, HWND, HGLOBAL, POINTL, S_OK,
};
use windows_sys::Win32::System::Com::{
    IAdviseSink, IDataObject, IEnumFORMATETC, IEnumSTATDATA, FORMATETC, STGMEDIUM, DVASPECT_CONTENT,
    IStream as SysIStream,
    TYMED_HGLOBAL,
    TYMED_ISTREAM,
};
use windows_sys::Win32::System::Com::StructuredStorage::GetHGlobalFromStream;
use windows_sys::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows_sys::Win32::System::Ole::{
    CF_HDROP, CF_TEXT, CF_UNICODETEXT, CF_OEMTEXT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE,
    DROPEFFECT_NONE, OleInitialize, RegisterDragDrop, ReleaseStgMedium, RevokeDragDrop,
};
use windows_sys::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindow, GW_CHILD, GW_HWNDNEXT};

fn clip_format_html() -> u16 {
    static F: OnceLock<u16> = OnceLock::new();
    *F.get_or_init(|| unsafe { RegisterClipboardFormatW(w!("HTML Format")) as u16 })
}

fn clip_format_uri_list() -> u16 {
    static F: OnceLock<u16> = OnceLock::new();
    *F.get_or_init(|| unsafe { RegisterClipboardFormatW(w!("text/uri-list")) as u16 })
}

/// 在源对象声明允许的 DROPEFFECT 中选一项 全 0 时按常见复制处理
fn drop_effect_from_allowed(allowed: u32) -> u32 {
    const MASK: u32 = DROPEFFECT_COPY | DROPEFFECT_LINK | DROPEFFECT_MOVE;
    let subset = if allowed == 0 {
        MASK
    } else {
        allowed & MASK
    };
    if subset & DROPEFFECT_COPY != 0 {
        DROPEFFECT_COPY
    } else if subset & DROPEFFECT_LINK != 0 {
        DROPEFFECT_LINK
    } else if subset & DROPEFFECT_MOVE != 0 {
        DROPEFFECT_MOVE
    } else {
        DROPEFFECT_NONE
    }
}

/// `SourceURL:` 行（CF_HTML 剪切板头里常见，拖超链接时多用此格式承载真实 URL）
fn extract_url_from_cf_html_headers(blob: &str) -> Option<String> {
    for line in blob.lines() {
        let t = line.trim();
        let Some(rest) = t.strip_prefix("SourceURL:") else {
            continue;
        };
        let u = rest.trim();
        if let Some(parsed) = crate::extract_http_url_from_text(u) {
            return Some(parsed);
        }
        if u.starts_with("http://") || u.starts_with("https://") {
            return Some(u.to_string());
        }
    }
    None
}

/// 曾注册过 OLE 拖放的 HWND（含子窗口），`Revoke` 后才会重新 `Register`，避免 GL 子窗体抢走命中
static OLE_REGISTERED_HWNDS: Mutex<Vec<isize>> = Mutex::new(Vec::new());

fn revoke_all_ole_targets() {
    if let Ok(mut g) = OLE_REGISTERED_HWNDS.lock() {
        for &h in g.iter() {
            unsafe {
                let _ = RevokeDragDrop(h as HWND);
            }
        }
        g.clear();
    }
}

unsafe fn collect_root_and_descendant_hwnds(root: HWND, out: &mut Vec<HWND>, seen: &mut HashSet<isize>) {
    let k = root as isize;
    if !seen.insert(k) {
        return;
    }
    out.push(root);
    let mut child = GetWindow(root, GW_CHILD);
    while child != 0 {
        collect_root_and_descendant_hwnds(child, out, seen);
        child = GetWindow(child, GW_HWNDNEXT);
    }
}

unsafe fn copy_hglobal_bytes(hg: HGLOBAL) -> Option<Vec<u8>> {
    if hg.is_null() {
        return None;
    }
    let p = GlobalLock(hg);
    if p.is_null() {
        return None;
    }
    let n = GlobalSize(hg) as usize;
    let v = std::slice::from_raw_parts(p.cast::<u8>(), n).to_vec();
    let _ = GlobalUnlock(hg);
    Some(v)
}

/// 从 `IStream` 读出全部字节 (`Stat` + `Read`)，不 Release 流（由 `ReleaseStgMedium` 负责）
unsafe fn read_istream_all(pstm: SysIStream) -> Option<Vec<u8>> {
    use windows::core::Interface;
    use windows::Win32::System::Com::{IStream as WinIStream, STATFLAG_NONAME, STATSTG};
    let stream = WinIStream::from_raw(pstm as *mut _);
    let mut stat = STATSTG::default();
    if stream.Stat(&mut stat, STATFLAG_NONAME).is_err() {
        std::mem::forget(stream);
        return None;
    }
    let len = stat.cbSize as usize;
    let mut v = vec![0u8; len];
    if len > 0 && stream.Read(v.as_mut_ptr().cast(), len as u32, None).is_err() {
        std::mem::forget(stream);
        return None;
    }
    std::mem::forget(stream);
    Some(v)
}

/// `GetData` 同时接受 `HGLOBAL` 与 `ISTREAM`（Chrome 等常以流形式提供 HTML / 文本）
unsafe fn data_object_format_bytes(data_obj: *const IDataObject, cf: u16) -> Option<Vec<u8>> {
    let fmt = FORMATETC {
        cfFormat: cf,
        ptd: ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT,
        lindex: -1,
        tymed: (TYMED_HGLOBAL as u32) | (TYMED_ISTREAM as u32),
    };
    let mut medium = std::mem::zeroed::<STGMEDIUM>();
    let vtbl = *(data_obj as *const *const IDataObjectVtbl);
    let hr = ((*vtbl).GetData)(data_obj as *mut IDataObject, &fmt, &mut medium);
    if hr < 0 {
        return None;
    }

    let out = match medium.tymed as i32 {
        t if t == TYMED_HGLOBAL => copy_hglobal_bytes(medium.u.hGlobal)?,
        t if t == TYMED_ISTREAM => {
            let pstm = medium.u.pstm;
            let mut hg = ptr::null_mut();
            let from_hg =
                if GetHGlobalFromStream(pstm, &mut hg) >= 0 && !hg.is_null() {
                    copy_hglobal_bytes(hg)
                } else {
                    None
                };
            match from_hg {
                Some(b) => b,
                None => read_istream_all(pstm)?,
            }
        }
        _ => {
            ReleaseStgMedium(&mut medium);
            return None;
        }
    };
    ReleaseStgMedium(&mut medium);
    Some(out)
}

/// `IUnknown` — `00000000-0000-0000-C000-000000000046`
const IID_IUNKNOWN: GUID = GUID {
    data1: 0x0000_0000,
    data2: 0x0000,
    data3: 0x0000,
    data4: [0xc0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};

/// `IDropTarget` — `00000122-0000-0000-C000-000000000046`
const IID_IDROPTARGET: GUID = GUID {
    data1: 0x0000_0122,
    data2: 0x0000,
    data3: 0x0000,
    data4: [0xc0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};

#[inline]
fn guid_eq(a: &GUID, b: &GUID) -> bool {
    a.data1 == b.data1
        && a.data2 == b.data2
        && a.data3 == b.data3
        && a.data4 == b.data4
}

// --- 与 winit `definitions.rs` 一致的 COM 布局（仅需 IDataObject::GetData） ---

#[repr(C)]
struct IUnknownVtbl {
    QueryInterface: unsafe extern "system" fn(
        This: *mut IUnknown,
        riid: *const GUID,
        ppvObject: *mut *mut c_void,
    ) -> HRESULT,
    AddRef: unsafe extern "system" fn(This: *mut IUnknown) -> u32,
    Release: unsafe extern "system" fn(This: *mut IUnknown) -> u32,
}

#[repr(C)]
struct IDataObjectVtbl {
    parent: IUnknownVtbl,
    GetData: unsafe extern "system" fn(
        This: *mut IDataObject,
        pformatetcIn: *const FORMATETC,
        pmedium: *mut STGMEDIUM,
    ) -> HRESULT,
    GetDataHere: unsafe extern "system" fn(
        This: *mut IDataObject,
        pformatetc: *const FORMATETC,
        pmedium: *mut STGMEDIUM,
    ) -> HRESULT,
    QueryGetData: unsafe extern "system" fn(This: *mut IDataObject, pformatetc: *const FORMATETC) -> HRESULT,
    GetCanonicalFormatEtc: unsafe extern "system" fn(
        This: *mut IDataObject,
        pformatetcIn: *const FORMATETC,
        pformatetcOut: *mut FORMATETC,
    ) -> HRESULT,
    SetData: unsafe extern "system" fn(
        This: *mut IDataObject,
        pformatetc: *const FORMATETC,
        pformatetcOut: *const FORMATETC,
        fRelease: BOOL,
    ) -> HRESULT,
    EnumFormatEtc: unsafe extern "system" fn(
        This: *mut IDataObject,
        dwDirection: u32,
        ppenumFormatEtc: *mut *mut IEnumFORMATETC,
    ) -> HRESULT,
    DAdvise: unsafe extern "system" fn(
        This: *mut IDataObject,
        pformatetc: *const FORMATETC,
        advf: u32,
        pAdvSInk: *const IAdviseSink,
        pdwConnection: *mut u32,
    ) -> HRESULT,
    DUnadvise: unsafe extern "system" fn(This: *mut IDataObject, dwConnection: u32) -> HRESULT,
    EnumDAdvise: unsafe extern "system" fn(
        This: *mut IDataObject,
        ppenumAdvise: *const *const IEnumSTATDATA,
    ) -> HRESULT,
}

#[repr(C)]
struct IDropTargetVtbl {
    parent: IUnknownVtbl,
    DragEnter: unsafe extern "system" fn(
        This: *mut IDropTarget,
        pDataObj: *const IDataObject,
        grfKeyState: u32,
        pt: *const POINTL,
        pdwEffect: *mut u32,
    ) -> HRESULT,
    DragOver: unsafe extern "system" fn(
        This: *mut IDropTarget,
        grfKeyState: u32,
        pt: *const POINTL,
        pdwEffect: *mut u32,
    ) -> HRESULT,
    DragLeave: unsafe extern "system" fn(This: *mut IDropTarget) -> HRESULT,
    Drop: unsafe extern "system" fn(
        This: *mut IDropTarget,
        pDataObj: *const IDataObject,
        grfKeyState: u32,
        pt: *const POINTL,
        pdwEffect: *mut u32,
    ) -> HRESULT,
}

#[repr(C)]
struct IDropTarget {
    lpVtbl: *const IDropTargetVtbl,
}

thread_local! {
    static TL_BRIDGE: RefCell<Option<Rc<DropBridge>>> = RefCell::new(None);
}

pub(crate) struct DropBridge {
    pub ui_weak: slint::Weak<crate::AppLauncher>,
    pub app_state: Rc<RefCell<crate::AppState>>,
}

fn drop_hint_status() -> slint::SharedString {
    slint::SharedString::from(
        "松开鼠标即可添加（支持 exe · lnk · url · 文件夹 · 图片 · 文档 · 含 http(s) 的文本）",
    )
}

fn with_bridge(f: impl FnOnce(&DropBridge)) {
    TL_BRIDGE.with(|b| {
        if let Some(br) = b.borrow().as_ref() {
            f(br);
        }
    });
}

fn ui_hover_file() {
    with_bridge(|br| {
        if let Some(ui) = br.ui_weak.upgrade() {
            ui.set_drop_active(true);
            ui.set_status_text(drop_hint_status());
        }
    });
}

fn ui_hover_cancel() {
    with_bridge(|br| {
        if let Some(ui) = br.ui_weak.upgrade() {
            ui.set_drop_active(false);
            ui.set_status_text(slint::SharedString::from(
                "拖入文件添加 · 左键启动 · 右键菜单",
            ));
        }
    });
}

fn dispatch_files(paths: Vec<PathBuf>) {
    with_bridge(|br| {
        if let Some(ui) = br.ui_weak.upgrade() {
            ui.set_drop_active(false);
            for p in paths {
                crate::add_dropped_path(&ui, &br.app_state, p);
            }
        }
    });
}

fn dispatch_url(url: String) {
    with_bridge(|br| {
        if let Some(ui) = br.ui_weak.upgrade() {
            ui.set_drop_active(false);
            crate::add_dropped_http_url(&ui, &br.app_state, url);
        }
    });
}

/// 文本拖放（无 HDROP）：含 http(s) 的走 [`dispatch_url`]，其余在状态栏给出预览，避免禁止光标却无反馈
fn dispatch_plain_text(text: String) {
    with_bridge(|br| {
        if let Some(ui) = br.ui_weak.upgrade() {
            ui.set_drop_active(false);
            let mut preview: String = text.chars().take(120).collect();
            if text.chars().count() > 120 {
                preview.push('…');
            }
            let one_line = preview.replace(['\r', '\n'], " ");
            ui.set_status_text(slint::SharedString::from(format!(
                "已接收文本（未识别为链接）：{one_line}"
            )));
        }
    });
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TextDropKind {
    Url,
    Plain,
}

#[repr(C)]
struct LauncherDropHandlerData {
    interface: IDropTarget,
    refcount: AtomicUsize,
    window: HWND,
    cursor_effect: u32,
    hovered_is_valid: bool,
    hover_text: Option<TextDropKind>,
}

pub struct LauncherDropHandler {
    pub data: *mut LauncherDropHandlerData,
}

#[allow(non_snake_case)]
impl LauncherDropHandler {
    pub fn new(window: HWND) -> LauncherDropHandler {
        let data = Box::new(LauncherDropHandlerData {
            interface: IDropTarget {
                lpVtbl: &DROP_TARGET_VTBL as *const IDropTargetVtbl,
            },
            refcount: AtomicUsize::new(1),
            window,
            cursor_effect: DROPEFFECT_NONE,
            hovered_is_valid: false,
            hover_text: None,
        });
        LauncherDropHandler {
            data: Box::into_raw(data),
        }
    }

    pub unsafe extern "system" fn QueryInterface(
        this: *mut IUnknown,
        riid: *const GUID,
        ppv_object: *mut *mut c_void,
    ) -> HRESULT {
        if ppv_object.is_null() {
            return E_POINTER;
        }
        unsafe {
            *ppv_object = ptr::null_mut();
            if riid.is_null() {
                return E_NOINTERFACE;
            }
            let rid = *riid;
            if guid_eq(&rid, &IID_IUNKNOWN) || guid_eq(&rid, &IID_IDROPTARGET) {
                *ppv_object = this as *mut c_void;
                let _count = LauncherDropHandler::AddRef(this);
                return S_OK;
            }
            E_NOINTERFACE
        }
    }

    pub unsafe extern "system" fn AddRef(this: *mut IUnknown) -> u32 {
        let drop_handler_data = unsafe { Self::from_interface(this) };
        let count = drop_handler_data.refcount.fetch_add(1, Ordering::Release) + 1;
        count as u32
    }

    pub unsafe extern "system" fn Release(this: *mut IUnknown) -> u32 {
        let drop_handler = unsafe { Self::from_interface(this) };
        let count = drop_handler.refcount.fetch_sub(1, Ordering::Release) - 1;
        if count == 0 {
            drop(unsafe { Box::from_raw(this as *mut LauncherDropHandlerData) });
        }
        count as u32
    }

    pub unsafe extern "system" fn DragEnter(
        this: *mut IDropTarget,
        pDataObj: *const IDataObject,
        _grfKeyState: u32,
        _pt: *const POINTL,
        pdwEffect: *mut u32,
    ) -> HRESULT {
        let drop_handler = unsafe { Self::from_interface(this) };
        drop_handler.hovered_is_valid = false;
        drop_handler.hover_text = None;
        drop_handler.cursor_effect = DROPEFFECT_NONE;

        let had_hdrop = unsafe {
            Self::iterate_filenames(pDataObj, |_filename| {
                if !drop_handler.hovered_is_valid {
                    drop_handler.hovered_is_valid = true;
                    drop_handler.hover_text = None;
                    ui_hover_file();
                }
            })
        };

        if !had_hdrop {
            if unsafe { try_extract_http_url_from_data_object(pDataObj) }.is_some() {
                drop_handler.hovered_is_valid = true;
                drop_handler.hover_text = Some(TextDropKind::Url);
                ui_hover_file();
            } else if unsafe { try_extract_any_text_from_data_object(pDataObj) }.is_some() {
                drop_handler.hovered_is_valid = true;
                drop_handler.hover_text = Some(TextDropKind::Plain);
                ui_hover_file();
            }
        }

        let incoming = if pdwEffect.is_null() {
            DROPEFFECT_COPY | DROPEFFECT_LINK | DROPEFFECT_MOVE
        } else {
            unsafe { *pdwEffect }
        };
        let effect = if drop_handler.hovered_is_valid {
            drop_effect_from_allowed(incoming)
        } else {
            DROPEFFECT_NONE
        };
        drop_handler.cursor_effect = effect;
        if !pdwEffect.is_null() {
            unsafe {
                *pdwEffect = effect;
            }
        }

        S_OK
    }

    pub unsafe extern "system" fn DragOver(
        this: *mut IDropTarget,
        _grfKeyState: u32,
        _pt: *const POINTL,
        pdwEffect: *mut u32,
    ) -> HRESULT {
        let drop_handler = unsafe { Self::from_interface(this) };
        let incoming = if pdwEffect.is_null() {
            DROPEFFECT_COPY | DROPEFFECT_LINK | DROPEFFECT_MOVE
        } else {
            unsafe { *pdwEffect }
        };
        let effect = if drop_handler.hovered_is_valid {
            drop_effect_from_allowed(incoming)
        } else {
            DROPEFFECT_NONE
        };
        drop_handler.cursor_effect = effect;
        if !pdwEffect.is_null() {
            unsafe {
                *pdwEffect = effect;
            }
        }
        S_OK
    }

    pub unsafe extern "system" fn DragLeave(this: *mut IDropTarget) -> HRESULT {
        let drop_handler = unsafe { Self::from_interface(this) };
        if drop_handler.hovered_is_valid {
            ui_hover_cancel();
        }
        drop_handler.hovered_is_valid = false;
        drop_handler.hover_text = None;
        drop_handler.cursor_effect = DROPEFFECT_NONE;
        S_OK
    }

    pub unsafe extern "system" fn Drop(
        this: *mut IDropTarget,
        pDataObj: *const IDataObject,
        _grfKeyState: u32,
        _pt: *const POINTL,
        _pdwEffect: *mut u32,
    ) -> HRESULT {
        let drop_handler = unsafe { Self::from_interface(this) };
        let text_kind = drop_handler.hover_text.take();
        drop_handler.hovered_is_valid = false;

        match text_kind {
            Some(TextDropKind::Url) => {
                if let Some(url) = unsafe { try_extract_http_url_from_data_object(pDataObj) } {
                    dispatch_url(url);
                    return S_OK;
                }
                ui_hover_cancel();
                return S_OK;
            }
            Some(TextDropKind::Plain) => {
                if let Some(s) = unsafe { try_extract_any_text_from_data_object(pDataObj) } {
                    dispatch_plain_text(s);
                } else {
                    ui_hover_cancel();
                }
                return S_OK;
            }
            None => {}
        }

        let mut paths = Vec::new();
        let had_hdrop =
            unsafe { Self::iterate_filenames(pDataObj, |filename| paths.push(filename)) };
        if had_hdrop {
            dispatch_files(paths);
        }
        ui_hover_cancel();

        S_OK
    }

    unsafe fn from_interface<'a, InterfaceT>(this: *mut InterfaceT) -> &'a mut LauncherDropHandlerData {
        unsafe { &mut *(this as *mut LauncherDropHandlerData) }
    }

    unsafe fn iterate_filenames<F>(data_obj: *const IDataObject, mut callback: F) -> bool
    where
        F: FnMut(PathBuf),
    {
        let drop_format = FORMATETC {
            cfFormat: CF_HDROP,
            ptd: ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT,
            lindex: -1,
            tymed: TYMED_HGLOBAL as u32,
        };

        let mut medium = unsafe { std::mem::zeroed::<STGMEDIUM>() };
        let vtbl = unsafe { *(data_obj as *const *const IDataObjectVtbl) };
        let get_data_result = unsafe {
            ((*vtbl).GetData)(data_obj as *mut IDataObject, &drop_format, &mut medium)
        };
        if get_data_result >= 0 {
            let hdrop = unsafe { medium.u.hGlobal as HDROP };

            let item_count = unsafe { DragQueryFileW(hdrop, 0xffffffff, ptr::null_mut(), 0) };

            for i in 0..item_count {
                let character_count =
                    unsafe { DragQueryFileW(hdrop, i, ptr::null_mut(), 0) as usize };
                let str_len = character_count + 1;
                let mut path_buf = Vec::with_capacity(str_len);
                unsafe {
                    DragQueryFileW(hdrop, i, path_buf.as_mut_ptr(), str_len as u32);
                    path_buf.set_len(str_len);
                }
                callback(OsString::from_wide(&path_buf[0..character_count]).into());
            }

            unsafe { ReleaseStgMedium(&mut medium) };
            true
        } else {
            false
        }
    }
}

impl Drop for LauncherDropHandler {
    fn drop(&mut self) {
        unsafe {
            LauncherDropHandler::Release(self.data as *mut IUnknown);
        }
    }
}

unsafe fn try_read_unicode_text(data_obj: *const IDataObject) -> Option<String> {
    let bytes = data_object_format_bytes(data_obj, CF_UNICODETEXT)?;
    if bytes.len() < 2 {
        return None;
    }
    let utf16: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let end = utf16.iter().position(|&c| c == 0).unwrap_or(utf16.len());
    Some(String::from_utf16_lossy(&utf16[..end]))
}

unsafe fn try_read_cf_text(data_obj: *const IDataObject) -> Option<String> {
    let bytes = data_object_format_bytes(data_obj, CF_TEXT)?;
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    Some(String::from_utf8_lossy(&bytes[..end]).into_owned())
}

unsafe fn try_read_cf_oemtext(data_obj: *const IDataObject) -> Option<String> {
    let bytes = data_object_format_bytes(data_obj, CF_OEMTEXT)?;
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    Some(String::from_utf8_lossy(&bytes[..end]).into_owned())
}

unsafe fn try_read_cf_html_as_string(data_obj: *const IDataObject) -> Option<String> {
    let id = clip_format_html();
    if id == 0 {
        return None;
    }
    let bytes = data_object_format_bytes(data_obj, id)?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

unsafe fn try_read_uri_list_url(data_obj: *const IDataObject) -> Option<String> {
    let id = clip_format_uri_list();
    if id == 0 {
        return None;
    }
    let bytes = data_object_format_bytes(data_obj, id)?;
    let s = String::from_utf8_lossy(&bytes);
    for line in s.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if let Some(u) = crate::extract_http_url_from_text(t) {
            return Some(u);
        }
    }
    None
}

unsafe fn try_extract_any_text_from_data_object(data_obj: *const IDataObject) -> Option<String> {
    let s = try_read_unicode_text(data_obj)
        .or_else(|| try_read_cf_text(data_obj))
        .or_else(|| try_read_cf_oemtext(data_obj))
        .or_else(|| try_read_cf_html_as_string(data_obj))?;
    if s.trim().is_empty() {
        return None;
    }
    Some(s)
}

unsafe fn try_extract_http_url_from_data_object(data_obj: *const IDataObject) -> Option<String> {
    if let Some(s) = try_read_unicode_text(data_obj) {
        if let Some(u) = crate::extract_http_url_from_text(&s) {
            return Some(u);
        }
    }
    if let Some(s) = try_read_cf_text(data_obj) {
        if let Some(u) = crate::extract_http_url_from_text(&s) {
            return Some(u);
        }
    }
    if let Some(s) = try_read_cf_oemtext(data_obj) {
        if let Some(u) = crate::extract_http_url_from_text(&s) {
            return Some(u);
        }
    }
    if let Some(s) = try_read_cf_html_as_string(data_obj) {
        if let Some(u) = extract_url_from_cf_html_headers(&s) {
            return Some(u);
        }
        if let Some(u) = crate::extract_http_url_from_text(&s) {
            return Some(u);
        }
    }
    try_read_uri_list_url(data_obj)
}

static DROP_TARGET_VTBL: IDropTargetVtbl = IDropTargetVtbl {
    parent: IUnknownVtbl {
        QueryInterface: LauncherDropHandler::QueryInterface,
        AddRef: LauncherDropHandler::AddRef,
        Release: LauncherDropHandler::Release,
    },
    DragEnter: LauncherDropHandler::DragEnter,
    DragOver: LauncherDropHandler::DragOver,
    DragLeave: LauncherDropHandler::DragLeave,
    Drop: LauncherDropHandler::Drop,
};

/// 在根 HWND 及其**所有子 HWND** 上注册 OLE `IDropTarget`（OpenGL 等常以子窗体为命中目标）
pub fn mount_launcher_ole_drop_target(
    hwnd_root: isize,
    ui_weak: slint::Weak<crate::AppLauncher>,
    app_state: Rc<RefCell<crate::AppState>>,
) -> Result<(), String> {
    unsafe {
        let ole = OleInitialize(ptr::null());
        if ole < 0 {
            return Err(format!("OleInitialize 失败: HRESULT 0x{:08X}", ole as u32));
        }
    }

    TL_BRIDGE.with(|b| {
        *b.borrow_mut() = Some(Rc::new(DropBridge {
            ui_weak,
            app_state,
        }));
    });

    revoke_all_ole_targets();

    let root = hwnd_root as HWND;
    let mut targets: Vec<HWND> = Vec::new();
    let mut seen = HashSet::new();
    unsafe {
        collect_root_and_descendant_hwnds(root, &mut targets, &mut seen);
    }

    let mut registered: Vec<isize> = Vec::new();
    for hwnd in targets {
        let handler = LauncherDropHandler::new(hwnd);
        let drop_target = unsafe { &mut (*handler.data).interface as *mut IDropTarget };
        let ptr = drop_target.cast();

        let mut hr = unsafe { RegisterDragDrop(hwnd, ptr) };
        if hr == DRAGDROP_E_ALREADYREGISTERED {
            unsafe {
                let _ = RevokeDragDrop(hwnd);
            }
            hr = unsafe { RegisterDragDrop(hwnd, ptr) };
        }

        if hr != S_OK {
            let _ = unsafe { LauncherDropHandler::Release(handler.data as *mut IUnknown) };
            eprintln!(
                "[ole-drop] RegisterDragDrop 跳过 HWND {:p}: HRESULT 0x{:08X}",
                hwnd as *const u8,
                hr as u32
            );
            continue;
        }

        registered.push(hwnd as isize);
        std::mem::forget(handler);
    }

    if registered.is_empty() {
        TL_BRIDGE.with(|b| b.borrow_mut().take());
        if let Ok(mut g) = OLE_REGISTERED_HWNDS.lock() {
            g.clear();
        }
        return Err(
            "RegisterDragDrop：没有任何 HWND 注册成功（子窗口或将在首帧后创建时可稍后再试）".into(),
        );
    }

    if let Ok(mut g) = OLE_REGISTERED_HWNDS.lock() {
        *g = registered;
    }

    Ok(())
}
