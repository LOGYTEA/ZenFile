//! 三层配置模型（技术方案 §14）。
//!
//! | 层 | 文件                                   | 优先级 |
//! |----|----------------------------------------|--------|
//! | 默认 | 编译进二进制 defaults.json             | 低     |
//! | 用户 | %APPDATA%\<app>\settings.json          | 中     |
//! | 覆盖 | %APPDATA%\<app>\settings.user.json     | 最高   |
//!
//! 硬规则：图形设置页只写"用户层"，永不覆盖"覆盖层"。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};

pub const CONFIG_VERSION: u32 = 1;
pub const APP_ID: &str = "ZenFile";

const DEFAULTS: &str = include_str!("defaults.json");

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub version: u32,
    pub core: CoreConfig,
    pub ui: UiConfig,
    pub files: FilesConfig,
    pub modules: std::collections::BTreeMap<String, ModuleConfig>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CoreConfig {
    pub language: String,
    pub theme: String,
    pub startup: StartupConfig,
    /// 键位：动作名 → "ctrl+alt+space" 形式
    pub hotkeys: std::collections::BTreeMap<String, String>,
    pub exit_to_tray: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StartupConfig {
    pub enabled: bool,
    /// tray | window
    pub mode: String,
    pub delay_seconds: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiConfig {
    pub statusbar_visible: bool,
    pub sidebar_visible: bool,
    pub sidebar_width: u32,
    pub titlebar_height: u32,
    /// 导航窗格是否**跟着当前文件夹自动展开并定位**。
    ///
    /// 默认 false：资源管理器默认也是不跟随的（展开过的地方才保持展开），
    /// 一路自动展开会瞬间把树撑得很长、还会顺带读很多层目录。
    pub sidebar_follow_current: bool,
    /// "此电脑"这个节点默认是否展开（展开后才看得到具体盘符）。
    pub sidebar_thispc_expanded: bool,
    pub tabs: TabsConfig,
    /// 圆角：键 = 圆角项 id（`fm_app::settings` 那棵三级树，如 "sidebar.hover"），
    /// 值 = DIP 半径。用 map 而不是二十来个字段 —— 这棵树还会继续长，
    /// 而且"没配过的项用默认值"正好靠 map 的缺省天然成立。
    #[serde(default)]
    pub radius: std::collections::BTreeMap<String, u32>,
    /// 「选中行首竖线」的形状（设置-交互页调整）。
    ///
    /// 竖线画在选中/焦点行的行首底色上：宽 = 水平厚度、高 = 纵向长度
    /// （0 = 跟行背景同高；**始终 clamp 在行背景内**，调大也不会悬空到
    /// 行背景外面）、stroke = 空心时的描边线宽、hollow = 只描边不填色。
    #[serde(default)]
    pub sel_bar: SelBarConfig,
    /// 上次退出时主窗口的位置与状态（第29轮：窗口位置持久化）。
    ///
    /// `w/h = 0` 表示"从没存过"，保持系统默认的创建位置。
    /// 坐标取自 `WINDOWPLACEMENT.rcNormalPosition`（还原态矩形），
    /// 恢复时也整套走 `SetWindowPlacement`，两边同一套坐标系不打架。
    #[serde(default)]
    pub window: WindowConfig,
    /// 图标主题：`default`（默认，直接调用系统 Shell 图标，零回归）/
    /// `optimized`（自绘优化套，第50轮后精简为两套之一）。
    ///
    /// 默认 default = 系统图标；optimized 由 `icon_theme` 模块参数化自绘。
    #[serde(default = "default_icon_theme")]
    pub icon_theme: String,
}

fn default_icon_theme() -> String {
    "default".to_string()
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct WindowConfig {
    #[serde(default)]
    pub x: i32,
    #[serde(default)]
    pub y: i32,
    #[serde(default)]
    pub w: i32,
    #[serde(default)]
    pub h: i32,
    /// 关窗口时是否处于最大化（恢复时一起还原）。
    #[serde(default)]
    pub maximized: bool,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self { x: 0, y: 0, w: 0, h: 0, maximized: false }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SelBarConfig {
    /// 水平厚度（DIP）
    #[serde(default = "default_sel_w")]
    pub w: u32,
    /// 高度（DIP，0 = 跟行背景同高）
    #[serde(default)]
    pub h: u32,
    /// 空心描边线宽（DIP，仅 hollow 时生效）
    #[serde(default = "default_sel_stroke")]
    pub stroke: u32,
    /// 空心：只描边不填色
    #[serde(default)]
    pub hollow: bool,
}

fn default_sel_w() -> u32 { 3 }
fn default_sel_stroke() -> u32 { 2 }

impl Default for SelBarConfig {
    fn default() -> Self {
        Self { w: 3, h: 0, stroke: 2, hollow: false }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TabsConfig {
    pub show_close_button: bool,
    pub max_tabs: u32,
    pub open_new_tab_on_double_click: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FilesConfig {
    pub view: ViewConfig,
    pub operations: OperationsConfig,
    pub thumbnails: ThumbnailConfig,
    pub navigation: NavigationConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ViewConfig {
    pub default_mode: String,
    pub show_hidden: bool,
    /// 显示"受保护的操作系统文件"（FILE_ATTRIBUTE_SYSTEM）。默认关闭，
    /// 与资源管理器"文件夹选项 → 查看"里的同名开关一致。
    pub show_system: bool,
    /// 是否显示文件扩展名。默认显示（比资源管理器更直白，避免"同名不同后缀"看不出来）。
    pub show_extensions: bool,
    /// 大小列的显示单位：`auto`（按最小单位自适应，见 app::human）/ `kb` / `mb` / `gb` / `bytes`。
    pub size_unit: String,
    pub sort_key: String,
    pub sort_descending: bool,
    /// 详细信息视图各列宽度（DIP，96 DPI 下的像素）。拖表头分隔线改的就是它。
    /// 默认值照资源管理器"详细信息"的默认列宽取：名称最宽、日期次之。
    pub col_name_w: u32,
    pub col_date_w: u32,
    pub col_kind_w: u32,
    pub col_size_w: u32,
    /// 每个文件夹手动切过的视图（第29轮：跨重启保持）。
    ///
    /// 键 = `norm_key` 归一化后的路径（小写、反斜杠、无尾斜杠），
    /// 值 = `view_name`（`details`/`list`/`medium`/`large`）。
    /// 之前只存在内存 HashMap 里，重启即丢 —— 用户反馈"文件夹的
    /// 布局样式压根不会保存"。资源管理器 bag 只在
    /// `default_mode == "system"` 时才被读回，靠不住。
    #[serde(default)]
    pub overrides: std::collections::BTreeMap<String, String>,
    /// **每个文件夹各自的列布局**（第33轮：列头右键自定义，Windows 逻辑）。
    ///
    /// 键 = `norm_key` 归一化后的路径，值 = 该目录的列配置。
    /// 为什么按目录存：资源管理器的"选择列…"本来就是**每个文件夹各存一套**
    /// （存在注册表 bag 里）。我们照抄这个语义。
    #[serde(default)]
    pub col_layouts: std::collections::BTreeMap<String, ColLayout>,
}

/// 一个目录下的列布局：哪些列可见、多宽、什么顺序。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ColLayout {
    /// 可见的列（按显示顺序）。空 = 用默认四列（名称/修改日期/类型/大小）。
    /// 元素是稳定的列 id 字符串，见 `crate::columns::COL_ALL`。
    #[serde(default)]
    pub visible: Vec<String>,
    /// 各列宽度（DIP），键 = 列 id。缺项用该列的默认宽度。
    #[serde(default)]
    pub widths: std::collections::BTreeMap<String, u32>,
}

impl ViewConfig {
    pub fn size_unit_of(&self) -> crate::SizeUnit {
        crate::SizeUnit::parse(&self.size_unit)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OperationsConfig {
    /// shell | fast
    pub mode: String,
    pub fast_mode: bool,
    /// 快速模式线程池并发度（SSD 32-64 / HDD 16）
    pub worker_threads: usize,
    pub confirm_delete: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThumbnailConfig {
    pub enabled: bool,
    pub max_memory_mb: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NavigationConfig {
    /// 搜索结果硬上限，守住内存预算
    pub search_limit: usize,
    pub watch_current_dir: bool,
    /// 递归监听是被明确禁止的 —— 仅记录意图以便未来 TYPE-AWARE 扩展
    pub history_depth: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModuleConfig {
    pub enabled: bool,
    pub autostart: bool,
    #[serde(default)]
    pub settings: std::collections::BTreeMap<String, Value>,
}

impl Config {
    pub fn defaults() -> Result<Self> {
        let v: Value = serde_json::from_str(DEFAULTS)?;
        let c: Config = serde_json::from_value(v)?;
        Ok(c)
    }

    /// 依次叠加三层。后加载的层深度覆盖前一层的同名键。
    pub fn load(app_dir: &Path) -> Result<Self> {
        let mut merged: Value = serde_json::from_str(DEFAULTS)?;
        for name in ["settings.json", "settings.user.json"] {
            let path = app_dir.join(name);
            if path.exists() {
                let text = std::fs::read_to_string(&path)?;
                let layer: Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => {
                        // 配置损坏：备份并继续 ¿?使用上层 ¿（¿14.6）
                        let _ = std::fs::rename(&path, app_dir.join(format!("{name}.broken.bak")));
                        continue;
                    }
                };
                deep_merge(&mut merged, &layer);
            }
        }
        let cfg: Config = serde_json::from_value(merged)?;
        Ok(cfg.migrate())
    }

    fn migrate(mut self) -> Self {
        if self.version == 0 {
            self.version = CONFIG_VERSION;
        }
        if self.version != CONFIG_VERSION {
            // 未来新增版本在此逐版本升级；当前只有 v1，宽松接受并改写版本号。
            self.version = CONFIG_VERSION;
        }
        self
    }

    /// 写用户层。图形设置页唯一允许的写入入口。
    pub fn save_user_layer(app_dir: &Path, layer: &Value) -> Result<()> {
        std::fs::create_dir_all(app_dir)?;
        let path = app_dir.join("settings.json");
        let tmp = app_dir.join("settings.json.tmp");
        let text = serde_json::to_string_pretty(layer)?;
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)?; // 原子重命名，避免多实例竞态
        Ok(())
    }

    /// 局部写用户层：读现有 settings.json → 深合并 patch → 原子写回。
    /// 用于"只改一个开关"的场景，避免整层覆盖丢掉其它字段。
    pub fn save_user_patch(patch: &Value) -> Result<()> {
        let dir = app_dir()?;
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("settings.json");
        let mut merged: Value = if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(text) => serde_json::from_str(&text).unwrap_or(Value::Object(serde_json::Map::new())),
                Err(_) => Value::Object(serde_json::Map::new()),
            }
        } else {
            Value::Object(serde_json::Map::new())
        };
        deep_merge(&mut merged, patch);
        Self::save_user_layer(&dir, &merged)
    }

    pub fn hotkey(&self, action: &str) -> Option<&str> {
        self.core.hotkeys.get(action).map(|s| s.as_str())
    }

    pub fn module_enabled(&self, id: &str) -> bool {
        self.modules.get(id).map(|m| m.enabled).unwrap_or(false)
    }
}

fn deep_merge(base: &mut Value, patch: &Value) {
    match (base, patch) {
        (Value::Object(base_map), Value::Object(patch_map)) => {
            for (k, v) in patch_map {
                match base_map.get_mut(k) {
                    Some(existing) => deep_merge(existing, v),
                    None => {
                        base_map.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (base_val, patch_val) => {
            *base_val = patch_val.clone();
        }
    }
}

/// 应用数据目录：%APPDATA%\<APP_ID>
pub fn app_dir() -> Result<PathBuf> {
    let appdata = std::env::var("APPDATA").map_err(|_| Error::Config("APPDATA 未定义".into()))?;
    Ok(PathBuf::from(appdata).join(APP_ID))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse() {
        let c = Config::defaults().unwrap();
        assert_eq!(c.version, 1);
        assert!(c.core.hotkeys.contains_key("global.show_window"));
    }

    #[test]
    fn user_layer_overrides_defaults() {
        let mut base: Value = serde_json::from_str(DEFAULTS).unwrap();
        let patch: Value = serde_json::from_str(r#"{"files":{"view":{"sort_descending":true}}}"#).unwrap();
        deep_merge(&mut base, &patch);
        let c: Config = serde_json::from_value(base).unwrap();
        assert!(c.files.view.sort_descending);
    }
}
