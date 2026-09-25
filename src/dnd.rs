//! 往资源管理器里拖项目：先在临时目录造个 .lnk，再交给 OLE DoDragDrop
//!
//! 大概流程：
//! 1. 临时目录写一份指向真目标的 .lnk
//! 2. Shell 帮这个 .lnk 搞个 IDataObject
//! 3. DoDragDrop，鼠标交给系统，松手为止
//! 4. 资源管理器按 COPY 把 .lnk 拷过去 —— 看起来就是「生成快捷方式」

use std::path::{Path, PathBuf};

#[cfg(windows)]
pub use windows_impl::*;

#[cfg(not(windows))]
pub fn drag_app_link(_path: &Path, _name: &str, _temp_dir: &Path) -> Result<(), String> {
    Err(String::from("仅 Windows 支持"))
}

#[cfg(not(windows))]
pub fn create_shortcut(_target: &Path, _link: &Path, _args: &str, _work_dir: Option<&Path>) -> Result<(), String> {
    Err(String::from("仅 Windows 支持"))
}

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;
    use windows::core::{implement, Interface, HRESULT, PCWSTR};
    use windows::Win32::Foundation::{
        BOOL, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, HWND, S_OK,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, IDataObject, IPersistFile, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::System::Ole::{
        DoDragDrop, IDropSource, IDropSource_Impl, OleInitialize, DROPEFFECT, DROPEFFECT_COPY,
        DROPEFFECT_LINK,
    };
    use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
    use windows::Win32::UI::Shell::Common::ITEMIDLIST;
    use windows::Win32::UI::Shell::{
        IShellFolder, IShellLinkW, ILCreateFromPathW, ILFree, SHBindToParent, ShellLink,
    };

    const MK_LBUTTON: u32 = 0x0001;

    #[implement(IDropSource)]
    struct DropSource;

    impl IDropSource_Impl for DropSource_Impl {
        fn QueryContinueDrag(
            &self,
            escape: BOOL,
            key_state: MODIFIERKEYS_FLAGS,
        ) -> HRESULT {
            if escape.as_bool() {
                return DRAGDROP_S_CANCEL;
            }
            if (key_state.0 & MK_LBUTTON) == 0 {
                return DRAGDROP_S_DROP;
            }
            S_OK
        }

        fn GiveFeedback(&self, _effect: DROPEFFECT) -> HRESULT {
            DRAGDROP_S_USEDEFAULTCURSORS
        }
    }

    fn ensure_ole() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let _ = OleInitialize(None);
        }
    }

 /// 写一个 .lnk
    pub fn create_shortcut(
        target: &Path,
        link: &Path,
        args: &str,
        work_dir: Option<&Path>,
    ) -> Result<(), String> {
        ensure_ole();
        unsafe {
            let shell_link: IShellLinkW =
                CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
                    .map_err(|e| format!("CoCreateInstance ShellLink: {}", e))?;

            let target_w = wide(target.as_os_str());
            shell_link
                .SetPath(PCWSTR(target_w.as_ptr()))
                .map_err(|e| format!("SetPath: {}", e))?;

            if !args.is_empty() {
                let args_w = wide_str(args);
                shell_link
                    .SetArguments(PCWSTR(args_w.as_ptr()))
                    .map_err(|e| format!("SetArguments: {}", e))?;
            }

            let work = work_dir
                .map(|p| p.to_path_buf())
                .or_else(|| target.parent().map(|p| p.to_path_buf()));
            if let Some(w) = work {
                let w_wide = wide(w.as_os_str());
                let _ = shell_link.SetWorkingDirectory(PCWSTR(w_wide.as_ptr()));
            }

            let persist: IPersistFile = shell_link
                .cast()
                .map_err(|e| format!("Cast IPersistFile: {}", e))?;
            let link_w = wide(link.as_os_str());
            persist
                .Save(PCWSTR(link_w.as_ptr()), true)
                .map_err(|e| format!("Save lnk: {}", e))?;
        }
        Ok(())
    }

 /// 开拖 会卡住直到用户松手（OLE 就是这样）
    pub fn drag_app_link(target: &Path, name: &str, temp_dir: &Path) -> Result<(), String> {
        let _ = std::fs::create_dir_all(temp_dir);

        let safe_name = name
            .chars()
            .map(|c| match c {
                '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
                _ => c,
            })
            .collect::<String>();

        let link_path: PathBuf = temp_dir.join(format!("{}.lnk", safe_name));
        create_shortcut(target, &link_path, "", None)?;

        ensure_ole();

        unsafe {
            let wide_path = wide(link_path.as_os_str());
            let pidl: *const ITEMIDLIST = ILCreateFromPathW(PCWSTR(wide_path.as_ptr()));
            if pidl.is_null() {
                return Err(String::from("ILCreateFromPathW 返回空"));
            }

            let mut child: *mut ITEMIDLIST = ptr::null_mut();
            let parent: IShellFolder = match SHBindToParent::<IShellFolder>(
                pidl,
                Some(&mut child),
            ) {
                Ok(p) => p,
                Err(e) => {
                    ILFree(Some(pidl));
                    return Err(format!("SHBindToParent 失败：{}", e));
                }
            };

            let apidl = [child as *const ITEMIDLIST];
            let data_obj: IDataObject = match parent.GetUIObjectOf::<HWND, IDataObject>(
                HWND::default(),
                &apidl,
                None,
            ) {
                Ok(o) => o,
                Err(e) => {
                    ILFree(Some(pidl));
                    return Err(format!("GetUIObjectOf 失败：{}", e));
                }
            };

            let drop_source: IDropSource = DropSource.into();

            let mut effect = DROPEFFECT::default();
            let _ = DoDragDrop(
                &data_obj,
                &drop_source,
                DROPEFFECT_COPY | DROPEFFECT_LINK,
                &mut effect,
            );

            ILFree(Some(pidl));
        }

        let _ = std::fs::remove_file(&link_path);
        Ok(())
    }

    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }
    fn wide_str(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }
}
