use std::io::{BufWriter, Cursor};
use std::path::{Path, PathBuf};

use ico::{IconDir, IconDirEntry, IconImage, ResourceType};

/// 将各条目重写为 BMP，满足 rc.exe 对 ICONDIR「3.00」（实为 DIB）的要求。
fn write_bmp_only_ico(dir: IconDir, dst: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut out = IconDir::new(ResourceType::Icon);
    for ent in dir.entries() {
        let img = ent.decode()?;
        out.add_entry(IconDirEntry::encode_as_bmp(&img)?);
    }
    if out.entries().is_empty() {
        return Err("ico 内无任何图标条目".into());
    }
    let f = std::fs::File::create(dst)?;
    out.write(BufWriter::new(f))?;
    Ok(())
}

/// 解析标准 ICO，各帧重写为 BMP。
fn rewrite_valid_ico_to_bmp_entries(src: &Path, dst: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let raw = std::fs::read(src)?;
    let dir = IconDir::read(Cursor::new(raw))?;
    write_bmp_only_ico(dir, dst)
}

/// PNG/JPEG/BMP 裸文件（或扩展名错误）；不使用 image 的 ICO 解码器，以免损坏的 ICO 再次报错。
fn raster_bytes_to_single_bmp_ico(raw: &[u8], dst: &Path) -> Result<(), Box<dyn std::error::Error>> {
    use image::ImageFormat;

    let dyn_img = if raw.len() >= 4 && raw[..4] == [0x89, 0x50, 0x4E, 0x47] {
        image::load_from_memory_with_format(raw, ImageFormat::Png)?
    } else if raw.len() >= 3 && raw[..3] == [0xff, 0xd8, 0xff] {
        image::load_from_memory_with_format(raw, ImageFormat::Jpeg)?
    } else if raw.len() >= 2 && raw[..2] == [b'B', b'M'] {
        image::load_from_memory_with_format(raw, ImageFormat::Bmp)?
    } else {
        return Err(
            "无法作为 PNG/JPEG/BMP 解码。若为 ICO，请换用标准工具重新导出图标文件".into(),
        );
    };

    let img = dyn_img.into_rgba8();
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return Err("图像宽高无效".into());
    }
    let rgba = IconImage::from_rgba_data(w, h, img.into_raw());
    let mut dir = IconDir::new(ResourceType::Icon);
    dir.add_entry(IconDirEntry::encode_as_bmp(&rgba)?);
    let f = std::fs::File::create(dst)?;
    dir.write(BufWriter::new(f))?;
    Ok(())
}

fn make_rc_compatible_icon(src: &Path, dst: &Path) -> Result<(), Box<dyn std::error::Error>> {
    match rewrite_valid_ico_to_bmp_entries(src, dst) {
        Ok(()) => Ok(()),
        Err(e_ico) => {
            let raw = std::fs::read(src)?;
            raster_bytes_to_single_bmp_ico(&raw, dst).map_err(|e_img| {
                format!(
                    "ICO 处理失败（{}）；栅格图降级失败（{}）",
                    e_ico, e_img
                )
            })?;
            Ok(())
        }
    }
}

fn main() {
    // exe 图标：项目根目录 ico.png（亦支持 ico.ico）。打包成 BMP ICO 再交给 rc.exe。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let manifest_dir =
            PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
        let src_icon = manifest_dir.join("ico.png");
        let out_icon =
            PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR")).join("ico_for_rc.ico");

        make_rc_compatible_icon(&src_icon, &out_icon).unwrap_or_else(|e| {
            panic!(
                "处理 ico.png 失败（{}）：请在项目根目录放置有效图标文件 ico.png（或 ICO/JPEG/BMP）",
                e
            );
        });

        // PerMonitorV2：进程启动即 DPI 感知，避免托盘/热键等先建 HWND 后
        // SetProcessDpiAwarenessContext 失败，导致系统位图缩放与鼠标命中错位。
        winresource::WindowsResource::new()
            .set_icon(
                out_icon
                    .to_str()
                    .expect("ico_for_rc.ico 路径需为有效 Unicode"),
            )
            .set("CompanyName", "QQ202173661")
            .set("ProductName", "亦安快速启动")
            .set("FileDescription", "亦安快速启动")
            .set_manifest(r#"
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0" xmlns:asmv3="urn:schemas-microsoft-com:asm.v3">
  <assemblyIdentity type="win32" name="YianLauncher" version="26.8.13.0"/>
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/>
    </dependentAssembly>
  </dependency>
  <asmv3:application>
    <asmv3:windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
    </asmv3:windowsSettings>
  </asmv3:application>
</assembly>
"#)
            .compile()
            .expect("嵌入 exe 图标失败：请确认已安装 Windows SDK（rc.exe）或 MinGW windres");
        println!("cargo:rerun-if-changed=ico.png");
    }

    slint_build::compile_with_config(
        "ui/app.slint",
        slint_build::CompilerConfiguration::new()
            .with_style("fluent-dark".into())
            .with_include_paths(vec!["ui".into()]),
    )
    .expect("Failed to compile Slint file");

    println!("cargo:rerun-if-changed=ui/app.slint");
    println!("cargo:rerun-if-changed=ui/settings_widgets.slint");

    // 将捆绑的 Everything 运行时复制到 target/{profile}/，与 exe 同目录
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        copy_everything_runtime_next_to_exe();
    }
}

fn copy_everything_runtime_next_to_exe() {
    let manifest =
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".into());
    let dest_dir = manifest.join("target").join(&profile);
    let src_dir = manifest.join("everything");
    if !src_dir.is_dir() {
        println!("cargo:warning=缺少 everything/ 运行时目录，计算机搜索将无法使用捆绑 Everything");
        return;
    }
    let _ = std::fs::create_dir_all(&dest_dir);
    let files = [
        "Everything.exe",
        "Everything64.dll",
        "es.exe",
        "Everything.ini",
        "Everything.lng",
    ];
    for name in files {
        let src = src_dir.join(name);
        if !src.is_file() {
            println!("cargo:warning=everything/ 缺少 {}", name);
            continue;
        }
        let dst = dest_dir.join(name);
        if let Err(e) = std::fs::copy(&src, &dst) {
            println!(
                "cargo:warning=复制 {} -> {} 失败: {}",
                src.display(),
                dst.display(),
                e
            );
        } else {
            println!("cargo:rerun-if-changed=everything/{}", name);
        }
    }
}
