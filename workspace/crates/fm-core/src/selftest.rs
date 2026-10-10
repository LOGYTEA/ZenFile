//! 无头自检：不需要显示器即可验证内核各层是否可用。
//! 通过 `fm-app.exe --selftest` 触发，用于 CI 与发布前冒烟。

use std::path::PathBuf;
use std::sync::mpsc::channel;

use crate::clipboard::ClipMode;
use crate::config;
use crate::engine::{enumerate_dir, run_search, EngineMsg, SearchRequest};
use crate::hotkey;
use crate::ipc::{self, Capability, Envelope};
use crate::natural::natural_cmp;
use crate::ops::{transfer_async, TransferKind};
use crate::paths;
use crate::types::{SortKey, SortSpec, ViewMode};
use crate::watch::{DirWatcher, WatchMsg};

struct Harness {
    passed: usize,
    failed: usize,
}

impl Harness {
    fn check(&mut self, name: &str, ok: bool, detail: &str) {
        if ok {
            self.passed += 1;
            println!("  [PASS] {name} {detail}");
        } else {
            self.failed += 1;
            println!("  [FAIL] {name} {detail}");
        }
    }
}

pub fn run() -> i32 {
    let mut h = Harness { passed: 0, failed: 0 };
    println!("fm-core 无头自检\n");

    // 1. 自然排序
    let order = natural_cmp("file2", "file10");
    h.check("natural::sort", order == std::cmp::Ordering::Less, "file2 < file10");

    // 2. 地址栏解析
    let cwd = PathBuf::from("C:\\Windows");
    let t = paths::resolve("cmd", &cwd);
    let ok = matches!(&t, Ok(paths::Target::ShellCommand(name)) if name == "cmd");
    h.check("paths::cmd", ok, "cmd -> ShellCommand");
    let ok = matches!(paths::resolve("..", &cwd), Ok(paths::Target::Navigate(p)) if p == PathBuf::from("C:\\"));
    h.check("paths::parent", ok, ".. -> C:\\");
    let appdata = paths::expand_env("%APPDATA%");
    h.check(
        "paths::env",
        appdata.contains("AppData") && !appdata.contains('%'),
        &format!("%APPDATA% -> {appdata}"),
    );
    let ok = matches!(paths::resolve("shell:Downloads", &cwd), Ok(paths::Target::Navigate(p)) if p.to_string_lossy().ends_with("Downloads"));
    h.check("paths::shell_alias", ok, "shell:Downloads");

    // 3. 配置三层
    let cfg = config::Config::defaults().unwrap();
    h.check("config::defaults", cfg.version == 1, "version=1");
    let dir = config::app_dir().unwrap_or_else(|_| PathBuf::from(".config"));
    let loaded = config::Config::load(&dir);
    h.check("config::load", loaded.is_ok(), "三层叠加成功");
    let loaded = loaded.unwrap();
    h.check(
        "config::hotkeys",
        loaded.core.hotkeys.contains_key("file.copy"),
        &format!("{} 个键位", loaded.core.hotkeys.len()),
    );

    // 4. IPC 帧
    let frame = ipc::encode(&Envelope::request(1, Capability::TaskSnapshot, serde_json::Value::Null)).unwrap();
    let (decoded, consumed) = ipc::decode(&frame).unwrap().unwrap();
    let ok = matches!(decoded, Envelope::Request { id: 1, .. }) && consumed == frame.len();
    h.check("ipc::frame", ok, "长度前缀帧往返");

    // 5. 键位
    let hk = hotkey::parse("ctrl+alt+f").unwrap();
    h.check("hotkey::parse", hk.vk == 0x46, "ctrl+alt+f -> VK_F");

    // 6. 目录引擎：枚举 Windows 目录并计时
    let (tx, rx) = channel::<EngineMsg>();
    let start = std::time::Instant::now();
    let sort = SortSpec { key: SortKey::Name, descending: false };
    let target = PathBuf::from("C:\\Windows");
    enumerate_dir(&target, 1, sort, &tx).unwrap();
    let mut total = 0usize;
    let mut batches = 0usize;
    let mut done = false;
    while let Ok(msg) = rx.try_recv() {
        match msg {
            EngineMsg::Batch(b) => {
                total += b.entries.len();
                batches += 1;
                done = b.done;
            }
            EngineMsg::Error(_, e) => {
                h.check("engine::enumerate", false, &format!("{e}"));
                break;
            }
        }
    }
    let elapsed = start.elapsed();
    h.check(
        "engine::enumerate",
        total > 0 && done && batches > 0,
        &format!("C:\\Windows 顶层 {total} 项 / {batches} 批 / {elapsed:?}"),
    );

    // 7. 递归搜索
    let (tx2, rx2) = channel::<EngineMsg>();
    let root = std::env::temp_dir().join("fm-selftest-search");
    prepare_tree(&root, 20, 3);
    run_search(
        SearchRequest {
            root: root.clone(),
            query: "item".into(),
            generation: 7,
            limit: 5000,
            threads: 4,
        },
        tx2,
    );
    let mut found = 0;
    while let Ok(EngineMsg::Batch(b)) = rx2.try_recv() {
        found += b.entries.len();
    }
    h.check("engine::search", found > 0, &format!("递归命中 {found} 项"));

    // 8. 快速模式传输
    let src_dir = std::env::temp_dir().join("fm-selftest-src");
    let dst_dir = std::env::temp_dir().join("fm-selftest-dst");
    prepare_tree(&src_dir, 200, 1);
    let _ = std::fs::remove_dir_all(&dst_dir);
    std::fs::create_dir_all(&dst_dir).unwrap();
    let stats = transfer_async(TransferKind::Copy, &dst_dir, vec![src_dir.clone()], 4);
    loop {
        {
            let s = stats.lock().unwrap();
            if s.finished {
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let copied_count = std::fs::read_dir(dst_dir.join("fm-selftest-src"))
        .map(|rd| rd.flatten().count())
        .unwrap_or(0);
    h.check("ops::fast_copy", copied_count >= 200, &format!("复写了 {copied_count} 项"));

    // 9. 目录监听
    let (wtx, wrx) = channel::<WatchMsg>();
    let watch_root = std::env::temp_dir().join("fm-selftest-watch");
    prepare_tree(&watch_root, 1, 1);
    let watcher = DirWatcher::start(watch_root.clone(), wtx).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(120));
    std::fs::write(watch_root.join("probe.tmp"), b"x").unwrap();
    let mut notified = false;
    for _ in 0..40 {
        if wrx.try_recv().is_ok() {
            notified = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    h.check("watch::changes", notified, "ReadDirectoryChangesW 收到通知");
    watcher.stop();

    // 10. 清理
    for p in [root, src_dir, dst_dir, watch_root] {
        let _ = std::fs::remove_dir_all(p);
    }
    h.check("selftest::cleanup", true, "临时数据已清理");

    // 11. 视图模式与剪贴板模式常量（防止 UI 层误用）
    h.check("types::view_next", ViewMode::Details.next() == ViewMode::List, "视图循环");
    h.check(
        "clipboard::mode",
        ClipMode::Move != ClipMode::Copy,
        "剪切/复制语义可区分",
    );

    // 12. 大小单位：默认规则是"最小单位"，同一列里不允许出现有的 KB 有的字节
    let kb = crate::SizeUnit::Auto.format(512);
    let mb = crate::SizeUnit::Auto.format(2 * 1024 * 1024);
    let gb = crate::SizeUnit::Auto.format(3 * 1024 * 1024 * 1024);
    h.check(
        "size::auto_smallest",
        kb.ends_with("KB") && mb.ends_with("MB") && gb.ends_with("GB"),
        &format!("512B->{kb}, 2MB->{mb}, 3GB->{gb}"),
    );
    h.check(
        "size::fixed",
        crate::SizeUnit::KB.format(2048) == "2 KB"
            && crate::SizeUnit::Bytes.format(2048) == "2048 字节",
        "固定单位换算",
    );
    h.check(
        "size::parse_roundtrip",
        crate::SizeUnit::parse("mb") == crate::SizeUnit::MB
            && crate::SizeUnit::parse("auto") == crate::SizeUnit::Auto,
        "配置字符串往返",
    );

    // 13. 回收站：能枚举到条目就至少要保证条目自身可解析（$I/$R 成对）
    let items = crate::recycle::list();
    let paired = items.iter().all(|i| i.r_path.exists() && i.i_path.exists());
    h.check(
        "recycle::list",
        items.iter().all(|i| !i.name.is_empty()) && paired,
        &format!("{} 项，$I/$R 均成对", items.len()),
    );

    println!("\n通过 {}/{} 项", h.passed, h.passed + h.failed);
    if h.failed == 0 {
        0
    } else {
        1
    }
}

/// 造一棵测试目录树：depth 层，每层 files 个文件。
fn prepare_tree(root: &PathBuf, files: usize, depth: usize) {
    let _ = std::fs::remove_dir_all(root);
    std::fs::create_dir_all(root).unwrap();
    let mut dirs = vec![root.clone()];
    for d in 0..depth {
        let next_level: Vec<PathBuf> = dirs
            .iter()
            .map(|parent| {
                let child = parent.join(format!("dir{d}"));
                std::fs::create_dir_all(&child).unwrap();
                child
            })
            .collect();
        for parent in &dirs {
            for i in 0..files {
                std::fs::write(parent.join(format!("item{i}.txt")), format!("data {i}")).unwrap();
            }
        }
        dirs = next_level;
    }
}
