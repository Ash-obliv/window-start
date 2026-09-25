use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 启动器里的一条应用/快捷方式 字段基本都能在 UI 里改
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppInfo {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    #[serde(default)]
    pub icon_path: Option<PathBuf>,
    pub category_id: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub work_dir: Option<PathBuf>,
    #[serde(default)]
    pub keywords: Vec<String>,
 /// .url 解出来的网址，拿去拉 favicon 普通程序这里是 None
    #[serde(default)]
    pub web_url: Option<String>,
    #[serde(default)]
    pub exec_count: u32,
    #[serde(default)]
    pub last_used: Option<u64>,
    #[serde(default)]
    pub sort_order: i32,
    #[serde(default)]
    pub pinned: bool,
 /// 谁先置顶谁靠前 数字越小越顶
    #[serde(default)]
    pub pin_order: u64,
}

impl AppInfo {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        path: impl Into<PathBuf>,
        category_id: impl Into<String>,
    ) -> Self {
        let name: String = name.into();
        let id: String = id.into();
        Self {
            id,
            name: name.clone(),
            path: path.into(),
            icon_path: None,
            category_id: category_id.into(),
            description: None,
            args: Vec::new(),
            work_dir: None,
            keywords: vec![name.to_lowercase()],
            web_url: None,
            exec_count: 0,
            last_used: None,
            sort_order: 0,
            pinned: false,
            pin_order: 0,
        }
    }

    pub fn generate_id() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        format!("app_{}", timestamp)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Category {
    pub id: String,
    pub name: String,
    #[serde(default = "default_category_color")]
    pub color: String,
    #[serde(default)]
    pub sort_order: i32,
    #[serde(default = "default_true")]
    pub visible: bool,
 /// 分组密码锁 没设就是 None / 空串
    #[serde(default)]
    pub password: Option<String>,
}

impl Category {
    pub fn has_password(&self) -> bool {
        self.password
            .as_ref()
            .map(|p| !p.is_empty())
            .unwrap_or(false)
    }
}

fn default_category_color() -> String {
    String::from("#6366f1")
}

fn default_true() -> bool {
    true
}

/// 皮肤色系，UI 下拉里那几项
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum SkinId {
    #[default]
    #[serde(rename = "dark")]
    Dark,
    #[serde(rename = "light")]
    Light,
    #[serde(rename = "warm")]
    Warm,
    #[serde(rename = "cozy")]
    Cozy,
}

impl SkinId {
    pub fn from_index(i: i32) -> Self {
        match i {
            1 => SkinId::Light,
            2 => SkinId::Warm,
            3 => SkinId::Cozy,
            _ => SkinId::Dark,
        }
    }

    pub fn as_index(self) -> i32 {
        match self {
            SkinId::Dark => 0,
            SkinId::Light => 1,
            SkinId::Warm => 2,
            SkinId::Cozy => 3,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserConfig {
    #[serde(default)]
    pub categories: Vec<Category>,
    #[serde(default)]
    pub apps: Vec<AppInfo>,
    #[serde(default)]
    pub theme: Theme,
    #[serde(default)]
    pub window_geometry: Option<WindowGeometry>,
    #[serde(default)]
    pub hotkey: Option<String>,
 /// 全局热键开没开
    #[serde(default = "default_hotkey_enabled")]
    pub hotkey_enabled: bool,
 /// 网格图标多大（逻辑像素，大概 28～64）
    #[serde(default = "default_icon_size")]
    pub icon_size: u8,
    #[serde(default)]
    pub auto_start: bool,
    #[serde(default)]
    pub search_history: Vec<String>,
 /// 窗口钉在最前
    #[serde(default)]
    pub always_on_top: bool,
 /// 开机起来先缩到托盘
    #[serde(default)]
    pub start_minimized: bool,
 /// 贴边自动藏起来，鼠标凑过去再弹（有点像 QQ）
    #[serde(default)]
    pub edge_auto_hide: bool,
 /// 点一下启动就把主窗藏了
    #[serde(default)]
    pub hide_on_launch_click: bool,
 /// 主窗透明度，50～100，100 = 不透
    #[serde(default = "default_window_opacity")]
    pub window_opacity: u8,
 /// 当前皮肤
    #[serde(default)]
    pub skin: SkinId,
 /// 自定义背景图，有就用
    #[serde(default)]
    pub background_image: Option<PathBuf>,
 /// 标题栏上写啥名字
    #[serde(default = "default_launcher_title")]
    pub launcher_title: String,
}

fn default_launcher_title() -> String {
    String::from("亦安")
}

fn default_hotkey_enabled() -> bool {
    true
}

fn default_icon_size() -> u8 {
    46
}

fn default_window_opacity() -> u8 {
    100
}

impl Default for UserConfig {
    fn default() -> Self {
        Self {
            categories: vec![],
            apps: Vec::new(),
            theme: Theme::Dark,
            window_geometry: None,
            hotkey: None,
            hotkey_enabled: true,
            icon_size: default_icon_size(),
            auto_start: false,
            search_history: Vec::new(),
            always_on_top: false,
            start_minimized: false,
            edge_auto_hide: false,
            hide_on_launch_click: false,
            window_opacity: default_window_opacity(),
            skin: SkinId::Dark,
            background_image: None,
            launcher_title: default_launcher_title(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
pub enum Theme {
    #[default]
    #[serde(rename = "dark")]
    Dark,
    #[serde(rename = "light")]
    Light,
    #[serde(rename = "auto")]
    Auto,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowGeometry {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone)]
pub enum LaunchResult {
    Success,
    Failed(String),
    NotFound,
}
