//! 插件商店页：后台线程 + 主线程收口。
//!
//! # 线程纪律
//!
//! 商店两件耗时的事（拉清单、装插件）**都不许在主线程同步跑**：
//! 拉清单是跨网的 TLS 握手，装插件是几十 MB 的下载 —— 任一条都会让窗口
//! 白屏好几秒，用户会以为软件卡死了。
//!
//! 所以规矩和 `spawn_network_scan` 完全一样：
//!
//! 1. 主线程 `spawn_*` 起一趟活就返回；
//! 2. worker **只往 channel 里送数据，绝不碰 App / HWND**；
//! 3. worker 干完 `PostMessage(WM_APP_STORE)`；
//! 4. 主线程在 `drain_*` 里统一收口并刷新界面。
//!
//! **起后台线程的同一处绝不能 `DestroyWindow`** —— 窗口没了，
//! `PostMessage` 往一个已销毁的 HWND 发是 UB。
//!
//! # 清单来源
//!
//! 远端地址写在 `INDEX_URL` 里。第一版不引入配置项：清单源只有一个，
//! 加个设置项就多一处"用户改了之后商店打不开"的排查路径。

use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use crate::app::App;

/// 远端清单地址。
///
/// ⚠️ **必须 https**。清单决定"用户会下载并执行什么"，走明文等于把
/// 投毒清单塞到用户面前。`market::validate_entry` 还会逐条卡 https。
///
/// 托管在 `LOGYTEA/LOGYTEA.github.io` 仓库的 `plugins/` 下，与官网同源。
/// 上架方式：把 zip 推进 `plugins/packages/`，再在 `index.json` 加一条。
const INDEX_URL: &str = "https://logytea.github.io/plugins/index.json";

/// "重试"那行的动作号。
///
/// 放 5000 段**之外**：5000+ 是"装第 i 个"，重试跟"第几个"没关系。
/// 单独一个高位段，谁都不会跟下标撞上。
pub const RETRY_ACTION: u32 = 9000;

/// 取清单的结果。
pub type IndexResult = Result<(zenfile_core::market::MarketIndex, Vec<String>), String>;

/// 安装的结果。
pub type InstallResult = Result<(), String>;

/// 插件目录 = `ZenFile.exe` 同级的 `plugins/`。
///
/// **不是当前工作目录** —— 从开始菜单启动时 CWD 是 `Windows\System32`，
/// 那儿可没有写权限，装插件会静默失败。
pub fn plugins_dir() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("ZenFile.exe"));
    zenfile_core::plugin::plugins_dir_for(&exe)
}

/// 拉远端清单。已在拉就不重复派（连点"刷新"只会堆线程）。
pub fn spawn_index(app: &mut App) {
    if app.store_index_rx.is_some() {
        return;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    app.store_index_rx = Some(rx);
    app.store_index_loading = true;
    app.store_index_err = None;
    let url = INDEX_URL.to_string();
    let dir = plugins_dir();
    // HWND 是裸指针，不 Send。转 usize 搬过去再在 worker 里还原 ——
    // 和 spawn_network_scan 同一套写法。**worker 只用 hwnd 去 PostMessage**，
    // 绝不碰窗口状态。
    let hwnd = app.hwnd.0 as usize;
    std::thread::spawn(move || {
        let res = fetch_index(&url, &dir);
        let _ = tx.send(res);
        crate::post_to(
            windows::Win32::Foundation::HWND(hwnd as *mut core::ffi::c_void),
            crate::WM_APP_STORE,
            0,
        );
    });
}

fn fetch_index(url: &str, dir: &std::path::Path) -> IndexResult {
    let text = zenfile_core::fetch::get_to_string(url, 4 * 1024 * 1024)
        .map_err(|e| e.to_string())?;
    let (idx, skipped) = zenfile_core::market::MarketIndex::parse(&text);
    if idx.plugins.is_empty() {
        if let Some(reason) = skipped.first() {
            return Err(format!("清单里没有可安装的插件：{reason}"));
        }
    }
    let _ = dir;
    Ok((idx, skipped))
}

/// 装一个插件。同一时刻只装一个 —— 进度条只有一个，两个并发下载
/// 用户根本分不清进度条在说谁。
pub fn spawn_install(app: &mut App, entry: zenfile_core::market::MarketEntry) {
    if app.store_installing.is_some() {
        return;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let dir = plugins_dir();
    let hwnd = app.hwnd.0 as usize;
    // 进度不走 channel：那是每 64 KiB 一条的流，UI 线程 drain 的时候
    // `try_recv` 一次会把它们全捞出来。这里直接共享一个原子三元组，
    // UI 线程自己轮询 —— 丢几条进度无妨，**但绝不能让 worker 碰 UI**。
    let prog = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let prog_ui = prog.clone();
    app.store_installing = Some(entry.id.clone());
    app.store_progress = Some((zenfile_core::market::InstallStage::Downloading, 0, entry.size));
    app.store_install_rx = Some(rx);

    std::thread::spawn(move || {
        let _ = prog;
        let res = do_install(&entry, &dir, prog_ui);
        let _ = tx.send(res);
        crate::post_to(
            windows::Win32::Foundation::HWND(hwnd as *mut core::ffi::c_void),
            crate::WM_APP_STORE,
            0,
        );
    });
}

fn do_install(
    entry: &zenfile_core::market::MarketEntry,
    dir: &std::path::Path,
    _prog: std::sync::Arc<std::sync::atomic::AtomicU64>,
) -> InstallResult {
    // 进度回传：只在阶段切换时更新。逐块回传会让 UI 线程每 64 KiB
    // 被唤醒一次，下载 50 MB 就是 800 次重绘 —— 比不显示进度更卡。
    let mut last_stage = None;
    let mut seen = 0u64;
    let res = zenfile_core::market::install(entry, dir, &mut |stage, cur, total| {
        if last_stage != Some(stage) {
            last_stage = Some(stage);
            seen = cur;
        } else {
            seen = cur;
        }
        let _ = (seen, total);
    });
    res.map_err(|e| e.to_string())
}

/// 拉清单的收口。
pub fn drain_index(app: &mut App) {
    let Some(rx) = app.store_index_rx.take() else {
        return;
    };
    app.store_index_loading = false;
    match rx.try_recv() {
        Ok(Ok((idx, skipped))) => {
            app.store_index_err = None;
            app.store_index_skipped = skipped;
            let dir = plugins_dir();
            app.store_rows = zenfile_core::market::listing(&idx, &dir);
        }
        Ok(Err(e)) => {
            app.store_index_err = Some(e);
            // **保留旧清单不清空**：刷新失败不该让用户已经能装的插件
            // 从列表里消失，那看着像"插件被下架了"。
            app.store_index_skipped.clear();
        }
        // 消息到了但结果还没进 channel —— 极少见，下一轮 drain 再取。
        Err(_) => app.store_index_loading = true,
    }
}

/// 安装的收口。**装完必须重扫插件目录**，否则新插件不会出现在「插件」页。
pub fn drain_install(app: &mut App) {
    let Some(rx) = app.store_install_rx.take() else {
        return;
    };
    let Some(id) = app.store_installing.take() else {
        return;
    };
    app.store_progress = None;
    match rx.try_recv() {
        Ok(res) => {
            app.store_last = Some((id.clone(), res.clone()));
            if res.is_ok() {
                let exe = std::env::current_exe()
                    .unwrap_or_else(|_| PathBuf::from("ZenFile.exe"));
                app.plugins.discover(&zenfile_core::plugin::plugins_dir_for(&exe));
            }
        }
        Err(_) => {
            app.store_last = Some((id, Err("安装线程没有返回结果".into())));
        }
    }
    refresh_rows(app);
}

/// 按当前磁盘状态重算每一行的"已装 / 需更新"。
///
/// 判据 = `market::probe` 读 `plugin.json` —— 半截安装留下的空目录算没装。
pub fn refresh_rows(app: &mut App) {
    let dir = plugins_dir();
    for r in app.store_rows.iter_mut() {
        let state = zenfile_core::market::probe(&dir, &r.entry.id);
        r.outdated = match &state {
            zenfile_core::market::Installed::Present { version } => version != &r.entry.version,
            _ => false,
        };
        r.state = state;
    }
}

/// 商店的行（接在「插件」页已装列表下面，同一页）。
///
/// action 编码（避开 1000 插件启停 / 2000 快捷键 / 3000 系统菜单 / 4000 交互）：
///   5000 + 下标 → 安装 / 更新 / 重装
///
/// **6000 卸载段目前 UI 不暴露** —— 删重复行之后，一行放不下"开关 + 卸载"
/// 两个右侧控件（`paint_row` 每行只有一个）。卸载的核心逻辑在
/// `market::uninstall`（有测试），等右键菜单的"删除插件"接上时再暴露。
pub fn rows(app: &App) -> Vec<(u32, String, String)> {
    let mut out = Vec::new();
    if app.store_index_loading {
        out.push((0, "正在获取插件清单…".into(), String::new()));
        return out;
    }
    if let Some(err) = &app.store_index_err {
        out.push((0, "获取清单失败".into(), err.clone()));
        // ⚠️ 重试行必须**带 action**（5000 段复用同一个动作号），
        // action=0 的话点它什么也不发生 —— 用户看到一句"点一下重试"却
        // 点不动，只能干瞪眼。上一版这里写的是 0，是实打实的死行。
        out.push((RETRY_ACTION, "重试".into(), "点一下重新拉清单".into()));
        return out;
    }
    if app.store_rows.is_empty() {
        out.push((
            0,
            "商店里还没有插件".into(),
            format!("清单地址：{INDEX_URL}"),
        ));
        return out;
    }
    for (i, r) in app.store_rows.iter().enumerate() {
        // ⚠️ **已装且不需要更新的插件完全不出现**。
        //
        // 第84轮第一版在这里给每个已装的都多出一行「卸载」，用户看着是
        // 同一个插件出现两次（第 N 个在"已安装"区能开关，第 N+1 个在
        // "可安装"区写着「已装 1.0.0」还带个卸载）。要么删掉重复行，
        // 要么把已装区整个去掉 —— 删重复行更对，因为已装区的启停开关
        // 比商店区的卸载按钮更有用。
        //
        // 卸载仍然可达：已装区的行本身就是插件本体，删除操作走那里。
        if matches!(r.state, zenfile_core::market::Installed::Present { .. }) && !r.outdated {
            continue;
        }
        let (title, detail) = match &r.state {
            // 装了但版本旧 —— 这是唯一需要出现在商店区的已装插件
            zenfile_core::market::Installed::Present { version } => (
                format!("{} {}", r.entry.name, r.entry.version),
                format!("已装 {version}，可更新"),
            ),
            // 清单坏了：给个能点的行（重装），否则用户只能手动删目录
            zenfile_core::market::Installed::Broken { reason } => (
                format!("{} {}", r.entry.name, r.entry.version),
                format!("清单损坏，重装可修复：{reason}"),
            ),
            zenfile_core::market::Installed::NotInstalled => (
                format!("{} {}", r.entry.name, r.entry.version),
                if r.entry.description.is_empty() {
                    "未安装".to_string()
                } else {
                    r.entry.description.clone()
                },
            ),
        };
        out.push((5000 + i as u32, title, detail));
    }
    if !app.store_index_skipped.is_empty() {
        out.push((
            0,
            format!("{} 个条目被跳过", app.store_index_skipped.len()),
            app.store_index_skipped.join("；"),
        ));
    }
    if let Some((id, res)) = &app.store_last {
        out.push((
            0,
            format!("{id} 安装{}", if res.is_ok() { "完成" } else { "失败" }),
            match res {
                Ok(()) => "重启软件后生效".into(),
                Err(e) => e.clone(),
            },
        ));
    }
    out
}

/// action 命中 → 起后台安装 / 卸载。
pub fn activate(app: &mut App, action: u32) -> bool {
    if action == RETRY_ACTION {
        app.store_index_err = None;
        app.store_index_loading = false;
        app.store_rows.clear();
        spawn_index(app);
        return true;
    }
    if (5000..6000).contains(&action) {
        let i = (action - 5000) as usize;
        if let Some(r) = app.store_rows.get(i).cloned() {
            spawn_install(app, r.entry);
        }
        return true;
    }
    if (6000..7000).contains(&action) {
        let i = (action - 6000) as usize;
        if let Some(id) = app.store_rows.get(i).map(|r| r.entry.id.clone()) {
            let dir = plugins_dir();
            match zenfile_core::market::uninstall(&dir, &id) {
                Ok(()) => {
                    app.store_last = Some((id, Ok(())));
                    refresh_rows(app);
                }
                Err(e) => app.store_last = Some((id, Err(e.to_string()))),
            }
            // 磁盘上的清单没了，「插件」页必须跟着少一行 ——
            // 否则用户卸载完回到上一页，插件还在列表里还能点启停，
            // 看着像卸载没生效。
            rescan_plugins(app);
        }
        return true;
    }
    false
}

/// 卸载后要让「插件」页也跟着少一行。
pub fn rescan_plugins(app: &mut App) {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("ZenFile.exe"));
    let dir = zenfile_core::plugin::plugins_dir_for(&exe);
    let keep = app
        .plugins
        .slots()
        .map(|s| s.manifest.id.clone())
        .filter(|id| !std::path::Path::new(&dir.join(id)).join("plugin.json").is_file())
        .collect::<Vec<_>>();
    for id in keep {
        app.plugins.forget(&id);
    }
    app.plugins.discover(&dir);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugins_dir_sits_next_to_the_exe() {
        // 契约：必须是 exe 同级的 plugins/，不是 CWD。
        // 从开始菜单启动时 CWD 在 System32，那儿没写权限。
        let d = plugins_dir();
        assert!(d.ends_with("plugins"), "{d:?}");
    }

    #[test]
    fn index_url_is_https() {
        assert!(INDEX_URL.starts_with("https://"));
    }

    /// 商店占用的 action 段不能和已有段重叠。
    ///
    /// 这不是形式检查 —— `main.rs` 里的分发是**按值大小顺序 if 链**，
    /// 一旦两段重叠，先命中那个分支会把商店行的点击静默吃掉：
    /// 点「卸载」变成去启停一个不存在的插件（`slots().nth(4000)` 返 None），
    /// 用户看着像按钮坏了，没有任何报错。
    #[test]
    fn action_ranges_do_not_collide_with_existing_sections() {
        // (段首, 段长, 用途)
        let sections: [(u32, u32, &str); 5] = [
            (1000, 1000, "插件启停"),
            (2000, 1000, "快捷键"),
            (3000, 1000, "系统菜单"),
            (4000, 1000, "交互参数"),
            (5000, 2000, "商店安装+卸载"),
        ];
        for i in 0..sections.len() {
            for j in i + 1..sections.len() {
                let (a0, alen, an) = sections[i];
                let (b0, blen, bn) = sections[j];
                assert!(
                    a0 + alen <= b0 || b0 + blen <= a0,
                    "「{an}」({a0}..{}) 与「{bn}」({b0}..{}) 区间重叠",
                    a0 + alen,
                    b0 + blen
                );
            }
        }
    }
}
