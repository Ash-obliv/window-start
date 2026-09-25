use crate::models::{Category, UserConfig};
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// exe 旁边当根目录（便携），拿不到就凑合用 cwd
pub fn application_root_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("无法获取程序路径")?;
    let parent = exe.parent().filter(|p| !p.as_os_str().is_empty());
    parent
        .map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())
        .context("无法确定程序根目录")
}

#[derive(Clone)]
pub struct ConfigManager {
    config_dir: PathBuf,
    config_file: PathBuf,
    cache_dir: PathBuf,
}

impl ConfigManager {
    pub fn new() -> Result<Self> {
        let root = application_root_dir()?;
        let config_dir = root.clone();
        let cache_dir = root;
        let config_file = config_dir.join("config.json");

        Ok(Self {
            config_dir,
            config_file,
            cache_dir,
        })
    }

 /// 缺目录就建，启动时先喊一声比较安心
    pub fn ensure_dirs(&self) -> Result<()> {
        fs::create_dir_all(&self.config_dir)
            .context("创建配置目录失败")?;
        fs::create_dir_all(&self.cache_dir)
            .context("创建缓存目录失败")?;
        fs::create_dir_all(self.cache_dir.join("icons"))
            .context("创建图标缓存目录失败")?;
        fs::create_dir_all(self.config_dir.join("config"))
            .context("创建 config 子目录失败")?;
        fs::create_dir_all(self.custom_icons_dir())
            .context("创建自定义图标目录失败")?;
        Ok(())
    }

 /// 用户自己丢的图标：`<exe>/config/icons/`
    pub fn custom_icons_dir(&self) -> PathBuf {
        self.config_dir.join("config").join("icons")
    }

 /// 某个 app 对应的自定义 png 绝对路径
    pub fn custom_icon_path(&self, app_id: &str) -> PathBuf {
        self.custom_icons_dir()
            .join(format!("{}.png", sanitize_filename(app_id)))
    }

 /// 写进 config.json 的相对路径，搬家不碎
    pub fn custom_icon_rel_path(&self, app_id: &str) -> PathBuf {
        PathBuf::from("config")
            .join("icons")
            .join(format!("{}.png", sanitize_filename(app_id)))
    }

    pub fn background_image_path(&self) -> PathBuf {
        self.config_dir.join("config").join("background.png")
    }

    pub fn background_image_rel_path(&self) -> PathBuf {
        PathBuf::from("config").join("background.png")
    }

 /// 读配置 没有就造一份默认的写回去
    pub fn load_or_create_config_sync(&self) -> Result<UserConfig> {
        if !self.config_file.exists() {
            let default_config = Self::create_default_config();
            self.save_config_sync(&default_config)?;
            return Ok(default_config);
        }

        let content = fs::read_to_string(&self.config_file)
            .context("读取配置文件失败")?;

        let mut config: UserConfig = serde_json::from_str(&content)
            .context("解析配置文件失败")?;

 // 早年有个系统分类叫 "all"，现在不要了，底下的东西塞进第一个用户分组
        config.categories.retain(|c| c.id != "all");
        if config.categories.is_empty() {
            config.categories.push(Self::default_category());
        }
        let first_cat_id = config.categories[0].id.clone();
        for app in &mut config.apps {
            if app.category_id == "all" || app.category_id.is_empty() {
                app.category_id = first_cat_id.clone();
            }
        }

 // 分组按 sort_order 排一下
        config.categories.sort_by_key(|c| c.sort_order);

 // 应用：手动序 > 使用次数 > 名字
        config.apps.sort_by(|a, b| {
            b.sort_order
                .cmp(&a.sort_order)
                .then_with(|| b.exec_count.cmp(&a.exec_count))
                .then_with(|| a.name.cmp(&b.name))
        });

        Ok(config)
    }

 /// pretty 写回 config.json
    pub fn save_config_sync(&self, config: &UserConfig) -> Result<()> {
        self.ensure_dirs()?;

        let content = serde_json::to_string_pretty(config)
            .context("序列化配置失败")?;

        fs::write(&self.config_file, content)
            .context("写入配置文件失败")?;

        Ok(())
    }

    pub fn get_icon_cache_path(&self, app_id: &str) -> PathBuf {
        self.cache_dir
            .join("icons")
            .join(format!("{}.png", sanitize_filename(app_id)))
    }

 /// 网站图标缓存文件删掉就行，config 那边调用方自己改
    pub fn remove_cached_icon_for_app_id(&self, app_id: &str) {
        let p = self.get_icon_cache_path(app_id);
        let _ = fs::remove_file(p);
    }

    pub fn cache_dir(&self) -> &PathBuf {
        &self.cache_dir
    }

 /// 跟 exe 同级那一层，相对路径都拿它当基准
    pub fn config_dir_path(&self) -> PathBuf {
        self.config_dir.clone()
    }

    pub fn default_category() -> Category {
        Category {
            id: String::from("default"),
            name: String::from("默认"),
            color: String::from("#4a82c4"),
            sort_order: 0,
            visible: true,
            password: None,
        }
    }

    pub fn create_default_config() -> UserConfig {
        UserConfig {
            categories: vec![Self::default_category()],
            apps: Vec::new(),
            theme: crate::models::Theme::Dark,
            window_geometry: None,
            hotkey: None,
            hotkey_enabled: true,
            icon_size: 46,
            auto_start: false,
            always_on_top: false,
            start_minimized: false,
            edge_auto_hide: false,
            hide_on_launch_click: false,
            window_opacity: 100,
            skin: crate::models::SkinId::Dark,
            background_image: None,
            search_history: Vec::new(),
            launcher_title: crate::models::UserConfig::default().launcher_title,
        }
    }
}

/// 文件名里那些 Windows 不吃的字符换成 `_`
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            _ => c,
        })
        .collect::<String>()
        .replace(' ', "_")
}
