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
pub const APP_ID: &str = "NexFile";

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
    pub tabs: TabsConfig,
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
    pub sort_key: String,
    pub sort_descending: bool,
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
