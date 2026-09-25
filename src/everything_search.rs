//! Everything 全盘搜索：优先使用程序根目录捆绑的 Everything / es / SDK DLL

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::config::application_root_dir;

#[derive(Debug, Clone)]
pub struct EverythingHit {
    pub name: String,
 /// 完整路径
    pub path: PathBuf,
 /// 所在目录
    pub location: String,
 /// 修改时间显示
    pub modified: String,
 /// 属性显示（文件/文件夹 · 只读/隐藏…）
    pub attributes: String,
}

fn enrich_hit(name: String, path: PathBuf) -> EverythingHit {
    let location = path
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (modified, attributes) = match std::fs::metadata(&path) {
        Ok(meta) => {
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| {
                    let dt: chrono::DateTime<chrono::Local> = t.into();
                    Some(dt.format("%Y-%m-%d %H:%M").to_string())
                })
                .unwrap_or_else(|| "—".into());
            let attributes = format_file_attributes(&meta);
            (modified, attributes)
        }
        Err(_) => ("—".into(), "未知".into()),
    };
    EverythingHit {
        name,
        path,
        location,
        modified,
        attributes,
    }
}

fn format_file_attributes(meta: &std::fs::Metadata) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if meta.is_dir() {
        parts.push("文件夹");
    } else if meta.is_symlink() {
        parts.push("链接");
    } else {
        parts.push("文件");
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        let a = meta.file_attributes();
        if a & 0x1 != 0 {
            parts.push("只读");
        }
        if a & 0x2 != 0 {
            parts.push("隐藏");
        }
        if a & 0x4 != 0 {
            parts.push("系统");
        }
        if a & 0x20 != 0 {
            parts.push("存档");
        }
    }
    parts.join(" · ")
}

/// 管理员知道的主密码：清除分组密码锁时可用
pub const ADMIN_MASTER_PASSWORD: &str = "yian123";

fn log_line(msg: impl AsRef<str>) {
    let line = format!(
        "[{}] {}\n",
        chrono::Local::now().format("%H:%M:%S%.3f"),
        msg.as_ref()
    );
    let path = std::env::temp_dir().join("yian-everything.log");
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| {
            use std::io::Write;
            f.write_all(line.as_bytes())
        });
}

/// 程序根目录及 `everything` 子目录（按优先级）
fn program_search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(root) = application_root_dir() {
        dirs.push(root.clone());
        dirs.push(root.join("everything"));
    }
    if let Ok(cwd) = std::env::current_dir() {
        if !dirs.iter().any(|d| d == &cwd) {
            dirs.push(cwd.clone());
            dirs.push(cwd.join("everything"));
        }
    }
    dirs
}

fn find_in_program_dirs(file_name: &str) -> Option<PathBuf> {
    for dir in program_search_dirs() {
        let p = dir.join(file_name);
        if p.is_file() {
            return Some(p);
        }
 // Windows 大小写不敏感，再试常见写法
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for ent in rd.flatten() {
                if ent.file_name().eq_ignore_ascii_case(file_name) {
                    let p = ent.path();
                    if p.is_file() {
                        return Some(p);
                    }
                }
            }
        }
    }
    None
}

#[cfg(windows)]
fn find_everything_exe() -> Option<PathBuf> {
 // 1) 程序根目录捆绑（优先）
    if let Some(p) = find_in_program_dirs("Everything.exe") {
        log_line(format!("bundled Everything.exe = {}", p.display()));
        return Some(p);
    }

 // 2) 注册表 / 常见安装路径（仅作兜底）
    let mut candidates: Vec<PathBuf> = Vec::new();
    for root in [
        winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER),
        winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE),
    ] {
        if let Ok(key) = root.open_subkey(r"Software\voidtools\Everything") {
            for value_name in ["InstallLocation", "Path", "exe_path"] {
                if let Ok(v) = key.get_value::<String, _>(value_name) {
                    let p = PathBuf::from(v.trim_matches('"'));
                    if p.is_file() {
                        candidates.push(p);
                    } else {
                        let exe = p.join("Everything.exe");
                        if exe.is_file() {
                            candidates.push(exe);
                        }
                    }
                }
            }
        }
    }
    let pf = std::env::var_os("ProgramFiles").map(PathBuf::from);
    let pf86 = std::env::var_os("ProgramFiles(x86)").map(PathBuf::from);
    for base in [pf, pf86].into_iter().flatten() {
        let p = base.join("Everything\\Everything.exe");
        if p.is_file() {
            candidates.push(p);
        }
    }
    if let Some(path) = which_on_path("Everything.exe") {
        candidates.push(path);
    }
    candidates.into_iter().find(|p| p.is_file())
}

fn which_on_path(exe_name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let exe = dir.join(exe_name);
        if exe.is_file() {
            return Some(exe);
        }
    }
    None
}

#[cfg(windows)]
fn find_es_exe() -> Option<PathBuf> {
    if let Some(p) = find_in_program_dirs("es.exe") {
        return Some(p);
    }
    if let Some(everything) = find_everything_exe() {
        if let Some(dir) = everything.parent() {
            let es = dir.join("es.exe");
            if es.is_file() {
                return Some(es);
            }
        }
    }
    which_on_path("es.exe")
}

#[cfg(windows)]
fn find_sdk_dll() -> Option<PathBuf> {
    #[cfg(target_arch = "x86_64")]
    let names = ["Everything64.dll", "Everything.dll"];
    #[cfg(target_arch = "x86")]
    let names = ["Everything32.dll", "Everything.dll"];
    #[cfg(not(any(target_arch = "x86_64", target_arch = "x86")))]
    let names = ["Everything64.dll", "Everything.dll"];

    for name in names {
        if let Some(p) = find_in_program_dirs(name) {
            return Some(p);
        }
    }
    None
}

#[cfg(windows)]
fn discover_ipc_instance_names() -> Vec<Option<String>> {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetClassNameW};

    let mut names: Vec<Option<String>> = vec![None, Some(String::from("1.5a"))];

    struct Data {
        names: Vec<String>,
    }
    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let data = &mut *(lparam.0 as *mut Data);
        let mut buf = [0u16; 256];
        let len = GetClassNameW(hwnd, &mut buf);
        if len > 0 {
            let class = String::from_utf16_lossy(&buf[..len as usize]);
            if let Some(rest) = class.strip_prefix("EVERYTHING_TASKBAR_NOTIFICATION") {
                if let Some(inner) = rest
                    .strip_prefix("_(")
                    .and_then(|s| s.strip_suffix(')'))
                    .or_else(|| rest.strip_prefix("(").and_then(|s| s.strip_suffix(')')))
                {
                    if !inner.is_empty() {
                        data.names.push(inner.to_string());
                    }
                }
            }
        }
        BOOL(1)
    }

    let mut data = Data { names: Vec::new() };
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut data as *mut _ as isize));
    }
    for n in data.names {
        if !names.iter().any(|x| x.as_deref() == Some(n.as_str())) {
            names.push(Some(n));
        }
    }
    names
}

#[cfg(windows)]
fn try_open_client() -> Result<everything_ipc::wm::EverythingClient, String> {
    use everything_ipc::wm::EverythingClient;

    let mut last = String::from("NoIpcWindow");
    for inst in discover_ipc_instance_names() {
        match EverythingClient::with_instance(inst.as_deref()) {
            Ok(c) if c.is_ipc_available() => {
                log_line(format!("IPC client ok instance={inst:?}"));
                return Ok(c);
            }
            Ok(_) => last = format!("instance={inst:?} ipc unavailable"),
            Err(e) => last = format!("instance={inst:?} err={e}"),
        }
    }
    Err(last)
}

/// 从程序根目录启动捆绑的 Everything（便携、后台托盘）
#[cfg(windows)]
fn ensure_everything_running() -> Result<(), String> {
    if try_open_client().is_ok() {
        return Ok(());
    }

    let Some(exe) = find_everything_exe() else {
        log_line("Everything.exe missing in program root");
        return Err(String::from(
            "程序目录缺少 Everything.exe。请将 everything 文件夹中的文件放到与 yian-launcher.exe 同级目录",
        ));
    };
    log_line(format!("starting bundled {}", exe.display()));
    if let Some(dll) = find_sdk_dll() {
        log_line(format!("SDK dll present: {}", dll.display()));
    }

 // 在 Everything.exe 所在目录启动，确保读取同目录 Everything.ini（便携模式）
    let work_dir = exe.parent().map(Path::to_path_buf);
    let mut cmd = Command::new(&exe);
    cmd.arg("-startup");
    if let Some(dir) = work_dir {
        cmd.current_dir(dir);
    }
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.spawn()
        .map_err(|e| format!("无法启动捆绑 Everything（{}）：{e}", exe.display()))?;

    for i in 0..60 {
        std::thread::sleep(Duration::from_millis(200));
        if try_open_client().is_ok() {
            log_line(format!("IPC ready after {}ms", (i + 1) * 200));
            return Ok(());
        }
    }
    Err(String::from(
        "已启动程序目录下的 Everything，但仍无法连接。请稍候再搜，或手动运行同目录 Everything.exe",
    ))
}

#[cfg(windows)]
fn search_via_ipc(query: &str, max_results: u32) -> Result<Vec<EverythingHit>, String> {
    use everything_ipc::wm::{RequestFlags, Sort};

    ensure_everything_running()?;
    let client = try_open_client()?;

    for _ in 0..50 {
        if client.is_db_loaded() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let list = client
        .query_wait(query)
        .request_flags(
            RequestFlags::FullPathAndFileName
                | RequestFlags::FileName
                | RequestFlags::Path
                | RequestFlags::DateModified
                | RequestFlags::Attributes,
        )
        .sort(Sort::NameAscending)
        .max_results(max_results)
        .timeout(Duration::from_secs(25))
        .call()
        .map_err(|e| format!("Everything IPC 查询失败：{e}"))?;

    log_line(format!(
        "IPC query ok len={} total={}",
        list.len(),
        list.total_len()
    ));

    let mut out = Vec::with_capacity(list.len().min(max_results as usize));
    for item in list.iter() {
        let full = item
            .get_string(RequestFlags::FullPathAndFileName)
            .filter(|s| !s.is_empty());
        let (name, path) = if let Some(full) = full {
            let path = PathBuf::from(&full);
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| full.clone());
            (name, path)
        } else {
            let name = item
                .get_string(RequestFlags::FileName)
                .unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            let dir = item.get_string(RequestFlags::Path).unwrap_or_default();
            let path = if dir.is_empty() {
                PathBuf::from(&name)
            } else {
                Path::new(&dir).join(&name)
            };
            (name, path)
        };
        out.push(enrich_hit(name, path));
    }
    Ok(out)
}

#[cfg(windows)]
fn search_via_es(query: &str, max_results: u32) -> Result<Vec<EverythingHit>, String> {
    let Some(es) = find_es_exe() else {
        return Err(String::from("程序目录缺少 es.exe"));
    };
    log_line(format!("es.exe = {}", es.display()));

 // 确保捆绑的 Everything 在跑，es 才能查询
    let _ = ensure_everything_running();

    let work_dir = es.parent().map(Path::to_path_buf);
    let mut cmd = Command::new(&es);
    cmd.arg("-n")
        .arg(max_results.to_string())
        .arg("-hide-empty-search-results")
        .arg(query);
    if let Some(dir) = work_dir {
        cmd.current_dir(dir);
    }
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let output = cmd
        .output()
        .map_err(|e| format!("运行 es.exe 失败：{e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut out = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let path = PathBuf::from(line);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| line.to_string());
        out.push(enrich_hit(name, path));
        if out.len() as u32 >= max_results {
            break;
        }
    }
    if out.is_empty() && !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "es.exe 无结果 (exit={:?}) stderr={}",
            output.status.code(),
            err.trim()
        ));
    }
    log_line(format!("es.exe hits={}", out.len()));
    Ok(out)
}

#[cfg(windows)]
fn open_everything_ui(query: &str) -> Result<(), String> {
    let Some(exe) = find_everything_exe() else {
        return Err(String::from("程序目录缺少 Everything.exe"));
    };
    let work_dir = exe.parent().map(Path::to_path_buf);
    let mut cmd = Command::new(&exe);
    cmd.arg("-s").arg(query);
    if let Some(dir) = work_dir {
        cmd.current_dir(dir);
    }
    cmd.spawn()
        .map_err(|e| format!("无法打开 Everything 窗口：{e}"))?;
    Ok(())
}

#[cfg(windows)]
pub fn search_files(query: &str, max_results: u32) -> Result<Vec<EverythingHit>, String> {
    let q = query.trim();
    if q.is_empty() {
        return Ok(Vec::new());
    }
    log_line(format!(
        "search_files q={q:?} root={:?}",
        application_root_dir().ok()
    ));

    match search_via_ipc(q, max_results) {
        Ok(hits) if !hits.is_empty() => return Ok(hits),
        Ok(_) => log_line("IPC returned 0 hits, try es.exe"),
        Err(e) => log_line(format!("IPC failed: {e}")),
    }

    match search_via_es(q, max_results) {
        Ok(hits) => return Ok(hits),
        Err(e) => log_line(format!("es failed: {e}")),
    }

    match open_everything_ui(q) {
        Ok(()) => Err(String::from(
            "已用程序目录 Everything 打开搜索窗口。若首次运行请等待索引完成后再搜",
        )),
        Err(e) => Err(format!(
            "搜索失败：请确认程序目录存在 Everything.exe / Everything64.dll / es.exe。{e}"
        )),
    }
}

#[cfg(not(windows))]
pub fn search_files(_query: &str, _max_results: u32) -> Result<Vec<EverythingHit>, String> {
    Err(String::from("Everything 搜索仅在 Windows 上可用"))
}
