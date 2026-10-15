//! 插件运行时（技术方案 §3 / §10.2 的落地）。
//!
//! # 为什么插件是"同一个 exe 的另一个进程"而不是独立 exe
//!
//! 三条理由，缺一不可：
//!
//! 1. **崩溃隔离是硬需求**。插件跑在宿主进程里的话，一次 `panic` 或一个野指针
//!    就能把整个文件管理器带走 —— 用户正在整理文件，窗口突然没了，磁盘上的
//!    半截操作都无从追溯。所以插件**必须是独立进程**。
//! 2. **但不该是独立 exe**。每个插件一个 exe 意味着要单独分发、单独升级、
//!    单独签名，面板里还要处理"exe 版本与宿主不兼容"。改成同一个 exe 的
//!    另一个进程后：升级只有一次、版本永远一致、`CreateProcess` 传个
//!    `--plugin <id>` 就能区分身份。
//! 3. **进程树要能整体收掉**。所有插件都挂进同一个 Job Object
//!    （`KILL_ON_JOB_CLOSE`），宿主崩了/退了，它们跟着走，不会留下一堆孤儿。
//!
//! # 隔离的三层防线
//!
//! | 层 | 手段 | 挡住什么 |
//! |---|---|---|
//! | 1 | 每个插件一个进程 | 内存越界 / panic / 野指针 |
//! | 2 | 命名管道 + 长度前缀帧 | 插件往管道里灌垃圾把宿主搞崩 |
//! | 3 | 能力白名单（`ipc::Capability`） | 插件做它不该做的事 |
//!
//! 再加一条**看门狗**：插件必须周期心跳，超时（默认 5 秒）就判定卡死并重启。
//! 插件可以是"没崩但卡住了"（死循环、弹了个模态框），光靠"进程还在"完全
//! 发现不了 —— 那时候用户看到的就是一个点了没反应的插件。
//!
//! # 明确不做的事
//!
//! - **不 hook 宿主**。插件没有能力改宿主内存、装 API hook、读宿主函数表。
//!   想要新功能就改宿主代码重新编译，而不是塞一段机器码进去。
//! - **不在宿主线程上等插件**。所有 IPC 都是异步的：管道读不到完整帧就返回
//!   "还不够"，绝不允许在 UI 线程上阻塞等一个卡住的插件。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// 插件心跳超时（毫秒）。超过这么久没心跳就判定卡死。
///
/// 5 秒不是随便选的：Windows 默认 GUI 消息循环被系统挂起（比如用户拖动窗口
/// 跨屏触发 DWM 重建）可能占掉几百毫秒，1~2 秒会误杀正常插件。
pub const HEARTBEAT_TIMEOUT_MS: u64 = 5_000;

/// 插件自动重启次数上限。
///
/// **必须有上限**：一个一启动就崩的插件（配置写错、依赖缺失）如果无限重启，
/// 会变成"任务管理器里 ZenFile 疯狂拉进程"的灾难，比崩溃本身更糟。
/// 到上限后插件停在那儿，状态标成"启动失败"，让用户去面板上看日志。
pub const MAX_RESTARTS: u32 = 3;

/// 一个插件的声明。放在插件目录里每个插件的 `plugin.json`。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PluginManifest {
    /// 插件 id（唯一，主键）。建议用小写短横线：`clipboard-history`。
    pub id: String,
    /// 面板上显示的名字。
    pub name: String,
    /// 一句话说明，显示在名字下面。
    #[serde(default)]
    pub description: String,
    /// 协议版本。与 `crate::PROTOCOL_VERSION` 不一致则拒绝加载 ——
    /// 宁可告诉用户"这个插件要升级"，也不要让它跑在错位的协议上乱发消息。
    #[serde(default)]
    pub protocol_version: u32,
    /// 申请的能力。没申请的 capability，宿主直接拒。
    #[serde(default)]
    pub capabilities: Vec<crate::ipc::Capability>,
    /// 是否随宿主启动。用户在面板上关掉插件时改的是配置，这个字段是默认值。
    #[serde(default)]
    pub autostart: bool,
}

impl PluginManifest {
    /// 读一个 `plugin.json`。**任何一步出错都返回 Err**，调用方把该插件
    /// 标成"清单损坏"而不是跳过 —— 静默跳过的话用户根本不知道装了个啥。
    pub fn load(dir: &Path) -> Result<Self> {
        let file = dir.join("plugin.json");
        let text = std::fs::read_to_string(&file).map_err(|e| {
            Error::Other(format!("读不到 {}: {e}", file.display()))
        })?;
        let m: PluginManifest = serde_json::from_str(&text)
            .map_err(|e| Error::Other(format!("{} 解析失败: {e}", file.display())))?;
        if m.id.trim().is_empty() {
            return Err(Error::Other("plugin.json 缺 id".into()));
        }
        Ok(m)
    }
}

/// 插件运行时看到的某个插件的状态。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum PluginState {
    /// 配置里禁用，压根没启动。
    Disabled,
    /// 正在启动（进程已拉起，还没完成握手）。
    Starting,
    /// 正常运行。
    Running,
    /// 进程没了（正常退出或崩溃），等待重启或已达重启上限。
    Stopped,
    /// 卡死（心跳超时）。
    Hung,
    /// 连续崩溃到上限，不再重启。
    Failed,
}

impl PluginState {
    pub fn as_str(self) -> &'static str {
        match self {
            PluginState::Disabled => "已禁用",
            PluginState::Starting => "启动中",
            PluginState::Running => "运行中",
            PluginState::Stopped => "已停止",
            PluginState::Hung => "无响应",
            PluginState::Failed => "启动失败",
        }
    }
}

/// 一个插件的运行时记录。
pub struct PluginSlot {
    pub manifest: PluginManifest,
    pub state: PluginState,
    /// 进程 id。`0` = 没有进程。
    pub pid: u32,
    /// 命名管道名（`\\.\pipe\ZenFile.<pid>.<id>`）。
    pub pipe: String,
    /// 最近一次心跳的时刻（`GetTickCount64()`）。
    last_heartbeat: u64,
    /// 累计崩溃次数（含正常退出后的重启？不含，只有异常退出才算）。
    pub crashes: u32,
    /// 重启次数（本轮运行周期内）。
    restarts: u32,
    /// 清单 / 加载失败的原因，填在面板上给用户看。
    pub error: Option<String>,
    /// 用户是否要它跑。改这个要走 `set_enabled`，别直接翻。
    pub wanted: bool,
}

impl PluginSlot {
    fn new(manifest: PluginManifest, wanted: bool) -> Self {
        Self {
            pipe: String::new(),
            state: if wanted { PluginState::Stopped } else { PluginState::Disabled },
            pid: 0,
            last_heartbeat: 0,
            crashes: 0,
            restarts: 0,
            error: None,
            wanted,
            manifest,
        }
    }

    /// 这个插件现在是否有一个活着的进程。
    pub fn alive(&self) -> bool {
        self.pid != 0 && matches!(self.state, PluginState::Starting | PluginState::Running | PluginState::Hung)
    }
}

/// 插件运行时：管着一堆插件的生命周期。
///
/// **刻意不持有任何 UI 相关的东西**（`App` / `HWND`），所以它能在自检里
/// 直接跑，也能在没有窗口的托盘实例里跑。
pub struct PluginHost {
    /// 插件可执行文件（就是宿主自己）。所有插件都用它 + `--plugin <id>`。
    host_exe: PathBuf,
    /// 进程树（Job Object）。插件崩溃不影响它，宿主退了它带走全部插件。
    tree: crate::process::ProcessTree,
    slots: BTreeMap<String, PluginSlot>,
    /// 下一个请求 id。
    next_req_id: u64,
    /// 读插件管道的线程都归这一个 JoinHandle。
    readers: Vec<std::thread::JoinHandle<()>>,
}

impl PluginHost {
    pub fn new(host_exe: PathBuf) -> Self {
        // Job Object 建不起来不该让整个程序起不来（极少见，但要能降级）。
        // 降级后插件仍能跑，只是宿主崩了插件会变孤儿 —— 那也比"起不来"好。
        let tree = crate::process::ProcessTree::new().unwrap_or_default();
        Self {
            host_exe,
            tree,
            slots: BTreeMap::new(),
            next_req_id: 1,
            readers: Vec::new(),
        }
    }

    /// 扫描插件目录。每个子目录一个插件（读它的 `plugin.json`）。
    ///
    /// **清单损坏的也要收进来**（state=Failed + error），不能悄悄跳过 ——
    /// 用户装了个插件结果面板上什么都没有，他会以为是软件没扫到。
    pub fn discover(&mut self, plugins_dir: &Path) {
        let entries = match std::fs::read_dir(plugins_dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            // 没有 plugin.json 的目录不是插件（比如用户放了说明文档）
            if !dir.join("plugin.json").exists() {
                continue;
            }
            match PluginManifest::load(&dir) {
                Ok(m) => {
                    let wanted = m.autostart;
                    let id = m.id.clone();
                    let slot = PluginSlot::new(m, wanted);
                    self.slots.insert(id, slot);
                }
                Err(e) => {
                    // 用目录名当 id 占位，让它在面板上可见
                    let name = dir
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| "?".into());
                    let m = PluginManifest {
                        id: name.clone(),
                        name: format!("{name}（清单损坏）"),
                        description: String::new(),
                        protocol_version: 0,
                        capabilities: Vec::new(),
                        autostart: false,
                    };
                    let mut slot = PluginSlot::new(m, false);
                    slot.state = PluginState::Failed;
                    slot.error = Some(e.to_string());
                    self.slots.insert(name, slot);
                }
            }
        }
    }

    pub fn slots(&self) -> impl Iterator<Item = &PluginSlot> {
        self.slots.values()
    }

    pub fn slot(&self, id: &str) -> Option<&PluginSlot> {
        self.slots.get(id)
    }

    pub fn slot_mut(&mut self, id: &str) -> Option<&mut PluginSlot> {
        self.slots.get_mut(id)
    }

    /// 用户在面板上启 / 停一个插件。
    pub fn set_enabled(&mut self, id: &str, on: bool) -> Result<()> {
        let exe = self.host_exe.clone();
        let tree = &mut self.tree;
        let Some(slot) = self.slots.get_mut(id) else {
            return Err(Error::Other(format!("没有插件 {id}")));
        };
        // 清单损坏的插件不允许启 —— 它连 id 都没解析出来，进程参数凑不齐。
        if slot.state == PluginState::Failed && slot.error.is_some() {
            return Err(Error::Other(format!("插件清单损坏：{}", slot.error.clone().unwrap_or_default())));
        }
        slot.wanted = on;
        if on {
            start_slot(&exe, tree, slot);
        } else {
            stop_slot(slot);
        }
        Ok(())
    }

    /// 每帧调一次：检查心跳、该重启的重启、该回收的回收。
    ///
    /// **返回 true = 有状态变了，调用方该重画**。这是唯一的状态推进点，
    /// 放在主消息循环里按定时器调，不要散在别处。
    pub fn tick(&mut self) -> bool {
        let now = crate::now_ms();
        let mut dirty = false;
        for slot in self.slots.values_mut() {
            if !slot.wanted {
                continue;
            }
            match slot.state {
                PluginState::Running | PluginState::Hung | PluginState::Starting => {
                    // 先判"进程还在不在"，再判心跳。
                    //
                    // **顺序不能反**：崩溃退出的插件 pid 还在句柄表里，但
                    // `process_alive` 已经是 false 了。若先判心跳，一个已经
                    // 崩掉的插件会被记成"卡死 5 秒"，面板上写的是"无响应"，
                    // 而真相是"已崩溃"——排查方向都被带偏。
                    if !crate::process::process_alive(slot.pid) {
                        slot.pid = 0;
                        slot.crashes += 1;
                        // 进程没了但心跳刚来过 → 这是**正常退出**（插件自己关的）
                        // 还是崩溃？分不清，也不必分清：都按"重启一次"处理，
                        // 连续几次就放弃（`restart_or_fail` 里有上限）。
                        restart_or_fail(slot);
                        dirty = true;
                        continue;
                    }
                    let timeout = if slot.state == PluginState::Starting {
                        HEARTBEAT_TIMEOUT_MS * 2
                    } else {
                        HEARTBEAT_TIMEOUT_MS
                    };
                    if now.saturating_sub(slot.last_heartbeat) > timeout {
                        // 先把"卡死"这个状态亮出来给用户看，再杀掉重启 ——
                        // 面板上会短暂出现"无响应"，那是真实发生过的事。
                        slot.state = PluginState::Hung;
                        // 卡死和崩溃同等对待：杀掉 + 按上限重启。
                        kill_slot(slot);
                        slot.crashes += 1;
                        restart_or_fail(slot);
                        dirty = true;
                    }
                }
                _ => {}
            }
        }
        dirty
    }

    /// 把一个插件拉起来（若已在跑则什么都不做）。
    pub fn ensure_running(&mut self, id: &str) -> Result<()> {
        let exe = self.host_exe.clone();
        let tree = &mut self.tree;
        let slot = self
            .slots
            .get_mut(id)
            .ok_or_else(|| Error::Other(format!("没有插件 {id}")))?;
        if slot.alive() {
            return Ok(());
        }
        slot.wanted = true;
        start_slot(&exe, tree, slot);
        Ok(())
    }

    pub fn next_req_id(&mut self) -> u64 {
        let id = self.next_req_id;
        self.next_req_id += 1;
        id
    }

    /// 全部停掉。宿主退出前调。
    pub fn shutdown(&mut self) {
        for slot in self.slots.values_mut() {
            stop_slot(slot);
        }
        self.readers.clear();
    }
}

fn start_slot(exe: &Path, tree: &mut crate::process::ProcessTree, slot: &mut PluginSlot) {
    if slot.alive() {
        return;
    }
    // 协议版本对不上就不启：宁可面板上显示"不兼容"，也不要一个旧插件
    // 在新协议上乱发消息把宿主搞挂。
    if slot.manifest.protocol_version != 0
        && slot.manifest.protocol_version != crate::PROTOCOL_VERSION
    {
        slot.state = PluginState::Failed;
        slot.error = Some(format!(
            "协议版本不兼容：插件 {} vs 宿主 {}",
            slot.manifest.protocol_version,
            crate::PROTOCOL_VERSION
        ));
        return;
    }
    let args: [&str; 2] = ["--plugin", slot.manifest.id.as_str()];
    match tree.spawn(exe, &args) {
        Ok(pid) => {
            slot.pid = pid;
            slot.pipe = format!(r"\\.\pipe\ZenFile.{}.{}", pid, slot.manifest.id);
            slot.state = PluginState::Starting;
            slot.last_heartbeat = crate::now_ms();
            slot.error = None;
        }
        Err(e) => {
            slot.state = PluginState::Failed;
            slot.error = Some(format!("拉起失败：{e}"));
            slot.pid = 0;
        }
    }
}

fn stop_slot(slot: &mut PluginSlot) {
    kill_slot(slot);
    if !slot.wanted {
        slot.state = PluginState::Disabled;
    } else {
        slot.state = PluginState::Stopped;
    }
}

/// 结束插件进程（若还在）。
///
/// 这里**只置 pid/state，不做重启** —— 重启决策由 `restart_or_fail` 按崩溃
/// 次数决定。两者混在一起会导致"用户主动关闭"也被当成崩溃反复拉起。
fn kill_slot(slot: &mut PluginSlot) {
    if slot.pid == 0 {
        return;
    }
    crate::process::terminate_pid(slot.pid);
    slot.pid = 0;
    if slot.state != PluginState::Disabled {
        slot.state = PluginState::Stopped;
    }
}

/// 崩溃后决定重启还是放弃。**调用方负责先 `crashes += 1`**。
fn restart_or_fail(slot: &mut PluginSlot) {
    if slot.crashes > MAX_RESTARTS {
        slot.state = PluginState::Failed;
        slot.error = Some(format!("连续崩溃 {} 次，已停止重启（上限 {MAX_RESTARTS}）", slot.crashes));
        return;
    }
    slot.restarts += 1;
    slot.state = PluginState::Stopped;
    // 真正的拉起要等下一次 ensure_running / 启动流程 —— 这里**不直接 spawn**，
    // 因为拿不到 &mut tree（会与上面的迭代借用冲突）。
    // 统一由调用方在循环外再调一次 ensure_all()。
}

/// 把所有"要跑但没跑"的插件拉起来。`tick` 之后调一次。
pub fn ensure_all(host: &mut PluginHost) {
    let ids: Vec<String> = host
        .slots()
        .filter(|s| s.wanted && !s.alive() && s.state != PluginState::Failed)
        .map(|s| s.manifest.id.clone())
        .collect();
    for id in ids {
        let _ = host.ensure_running(&id);
    }
}

/// 插件目录的默认位置：`<exe 同级>/plugins`。
///
/// 放在 exe 旁边而不是 `%APPDATA%`，是为了让整个 ZenFile 目录可以直接
/// 拷走/绿色便携运行。
pub fn plugins_dir_for(exe: &Path) -> PathBuf {
    exe.parent().map(|p| p.join("plugins")).unwrap_or_else(|| PathBuf::from("plugins"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ZenFile.plugin_test.{name}.{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn manifest_roundtrip() {
        let d = tmp_dir("manifest");
        let sub = d.join("demo");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(
            sub.join("plugin.json"),
            r#"{"id":"demo","name":"演示","protocol_version":1}"#,
        )
        .unwrap();
        let m = PluginManifest::load(&sub).unwrap();
        assert_eq!(m.id, "demo");
        assert_eq!(m.name, "演示");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn missing_manifest_is_err_not_panic() {
        let d = tmp_dir("missing");
        assert!(PluginManifest::load(&d).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn broken_manifest_still_shows_up_in_panel() {
        // 清单是坏的，但插件**必须**出现在面板上（标 Failed），
        // 不能被 discover 静默跳过 —— 否则用户以为自己没装成功。
        let root = tmp_dir("broken");
        let sub = root.join("half-baked");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("plugin.json"), "{ this is not json").unwrap();
        let mut host = PluginHost::new(PathBuf::from("ZenFile.exe"));
        host.discover(&root);
        let s = host.slot("half-baked").expect("坏清单的插件也要收进来");
        assert_eq!(s.state, PluginState::Failed);
        assert!(s.error.is_some());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn restart_has_a_ceiling() {
        // 无限重启一个一启动就崩的插件比崩溃本身更糟，必须有上限。
        let mut slot = PluginSlot::new(
            PluginManifest {
                id: "x".into(),
                name: "x".into(),
                description: String::new(),
                protocol_version: crate::PROTOCOL_VERSION,
                capabilities: Vec::new(),
                autostart: true,
            },
            true,
        );
        for _ in 0..(MAX_RESTARTS + 3) {
            slot.crashes += 1;
            restart_or_fail(&mut slot);
        }
        assert_eq!(slot.state, PluginState::Failed);
        assert!(slot.crashes > MAX_RESTARTS);
    }

    #[test]
    fn disabled_plugin_is_not_wanted() {
        let mut slot = PluginSlot::new(
            PluginManifest {
                id: "y".into(),
                name: "y".into(),
                description: String::new(),
                protocol_version: 0,
                capabilities: Vec::new(),
                autostart: false,
            },
            false,
        );
        assert_eq!(slot.state, PluginState::Disabled);
        assert!(!slot.alive());
    }
}
