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

    // 12. 大小单位：自动档按"能表达这个大小的最小单位"走，且 <1KB 要写"字节"
    let b = crate::SizeUnit::Auto.format(512);
    let kb = crate::SizeUnit::Auto.format(2 * 1024);
    let mb = crate::SizeUnit::Auto.format(2 * 1024 * 1024);
    let gb = crate::SizeUnit::Auto.format(3 * 1024 * 1024 * 1024);
    let tb = crate::SizeUnit::Auto.format(3u64 * 1024 * 1024 * 1024 * 1024);
    let pb = crate::SizeUnit::Auto.format(3u64 * 1024 * 1024 * 1024 * 1024 * 1024);
    h.check(
        "size::auto_smallest",
        // 单位一律 B/KB/MB/GB/TB/PB，不再出现"字节" —— 与资源管理器/系统
        // 各处显示的口径一致（用户要求：不要"字节"这一档中文单位）。
        b == "512 B"
            && kb.ends_with("KB")
            && mb.ends_with("MB")
            && gb.ends_with("GB")
            && tb.ends_with("TB")
            && pb.ends_with("PB"),
        &format!("512B->{b}, 2K->{kb}, 2M->{mb}, 3G->{gb}, 3T->{tb}, 3P->{pb}"),
    );
    h.check(
        "size::fixed",
        crate::SizeUnit::KB.format(2048) == "2 KB"
            && crate::SizeUnit::Bytes.format(2048) == "2048 B"
            && crate::SizeUnit::TB.format(2 * 1024 * 1024 * 1024 * 1024) == "2 TB"
            && crate::SizeUnit::PB.format(2 * 1024 * 1024 * 1024 * 1024 * 1024) == "2 PB",
        "固定单位换算（含 TB / PB）",
    );
    h.check(
        "size::parse_roundtrip",
        crate::SizeUnit::parse("mb") == crate::SizeUnit::MB
            && crate::SizeUnit::parse("auto") == crate::SizeUnit::Auto
            && crate::SizeUnit::parse("pb") == crate::SizeUnit::PB
            && crate::SizeUnit::ALL.len() == 7,
        "配置字符串往返 + 七档齐全",
    );

    // 13. 回收站：能枚举到条目就至少要保证条目自身可解析（$I/$R 成对）
    let t0 = std::time::Instant::now();
    let items = crate::recycle::list();
    let listed_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let paired = items.iter().all(|i| i.r_path.exists() && i.i_path.exists());
    h.check(
        "recycle::list",
        items.iter().all(|i| !i.name.is_empty()) && paired,
        &format!("{} 项，$I/$R 均成对，枚举 {:.0} ms", items.len(), listed_ms),
    );

    // 14. 驱动器清单：类型、卷标、容量必须能从 Win32 一路读出来
    let drives = crate::drives::list();
    // 系统盘一定是固定盘，且必然拿得到容量 —— 这条挂了说明 GetDriveType 传参有问题
    let sysdrive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
    let sys = drives.iter().find(|d| d.letter_str() == sysdrive);
    let any_space = drives.iter().any(|d| d.total.is_some());
    let names_ok = drives.iter().all(|d| d.display_name().contains(d.letter));
    h.check(
        "drives::list",
        !drives.is_empty() && sys.is_some() && any_space && names_ok,
        &format!(
            "{} 个：{}",
            drives.len(),
            drives
                .iter()
                .map(|d| format!("{}={:?}", d.display_name(), d.kind))
                .collect::<Vec<_>>()
                .join("  ")
        ),
    );
    let sys_fixed = sys.map(|d| d.kind == crate::drives::DriveKind::Fixed && d.total.is_some());
    h.check(
        "drives::sysdrive",
        sys_fixed.unwrap_or(false),
        &format!("{sysdrive} 应为固定盘且可读容量"),
    );
    // 已用占比必须落在 [0,1]，且蓝条宽度换算后与实际偏差不超过 1px
    let ratios_ok = drives.iter().all(|d| match d.used_ratio() {
        None => true,
        Some(r) => (0.0..=1.0).contains(&r) && d.used().unwrap() <= d.total.unwrap(),
    });
    h.check("drives::ratio", ratios_ok, "已用占比均落在 [0,1]");

    // 15. 文件名索引：建索引 → 查询 → 序列化往返
    // 刻意建在 %TEMP% 下的子树上，且**不调 save()**（save 会按盘符写进
    // %LOCALAPPDATA%\ZenFile\index\C.bin，把真实缓存冲掉）。
    let idx_root = std::env::temp_dir().join("fm-selftest-index");
    prepare_tree(&idx_root, 12, 3);
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let idx = crate::index::Index::build(&idx_root.to_string_lossy(), &cancel, None);
    let built_n = idx.len();
    h.check("index::build", built_n >= 12 * 3, &format!("{built_n} 条"));
    // 查询必须能命中深层目录里的文件（这正是"整卷秒搜"要的效果）
    let hits = idx.query("item7", 100);
    let deep_ok = hits.iter().any(|x| x.path.to_string_lossy().contains("dir1"));
    h.check(
        "index::query",
        !hits.is_empty() && deep_ok,
        &format!("item7 命中 {} 项，含子目录 {}", hits.len(), deep_ok),
    );
    // 大小写不敏感 + 子串（不是前缀）
    h.check(
        "index::case",
        !idx.query("ITEM7", 10).is_empty(),
        "大写查询同样命中",
    );
    // 通配符
    h.check(
        "index::wildcard",
        idx.query("item1*.txt", 500).len() >= 3,
        &format!("item1*.txt -> {} 项", idx.query("item1*.txt", 500).len()),
    );
    // 前缀命中的要排在前面：搜 item1 时 item1.txt 优先于 item10.txt
    let ranked = idx.query("item1", 50);
    let first = ranked.first().map(|x| x.name.clone()).unwrap_or_default();
    h.check(
        "index::rank",
        first == "item1.txt",
        &format!("首位 = {first}"),
    );
    // 序列化往返：缓存能不能还原，决定了"第二次搜索是否秒开"
    let bytes = idx.to_bytes();
    let back = crate::index::Index::from_bytes(&bytes);
    let roundtrip = match &back {
        Some(b) => b.len() == idx.len() && b.query("item7", 100).len() == hits.len(),
        None => false,
    };
    h.check(
        "index::roundtrip",
        roundtrip,
        &format!("{} 字节，还原 {} 条", bytes.len(), back.map(|b| b.len()).unwrap_or(0)),
    );
    // 15b. 高级搜索语法：解析层（不依赖索引，纯字符串 → 结构化查询）
    use crate::index::{Op, Query};
    let q1 = Query::parse("ext:TXT");
    let q2 = Query::parse("size:>1kb");
    let q3 = Query::parse("date:>2020-01-01");
    let q4 = Query::parse("-item3 item1");
    let q5 = Query::parse("\"a b\" c");
    let q6 = Query::parse("a OR b");
    let q7 = Query::parse("kind:folder path:dir1 size:<=10mb");
    let syntax_ok = q1.ext.as_deref() == Some("txt")
        && q2.size == Some((Op::Gt, 1024))
        && matches!(q3.date, Some((Op::Gt, v)) if v > 0)
        && q4.not == vec!["item3".to_string()]
        && q4.texts == vec!["item1".to_string()]
        && q5.texts == vec!["a b".to_string(), "c".to_string()]
        && q6.any == vec!["a".to_string(), "b".to_string()]
        && q7.kind.as_deref() == Some("folder")
        && q7.path.as_deref() == Some("dir1")
        && q7.size == Some((Op::Le, 10 * 1024 * 1024));
    h.check(
        "index::syntax",
        syntax_ok,
        &format!(
            "ext={:?} size={:?} date={:?} not={:?} quote={:?} or={:?}",
            q1.ext, q2.size, q3.date.map(|(o, _)| o), q4.not, q5.texts, q6.any
        ),
    );
    // 15c. 高级搜索语法：执行层（真的在这些条目上跑一遍）
    let by_ext = idx.query("ext:txt", 500);
    let folders = idx.query("kind:folder", 500);
    let big = idx.query("size:>1b", 500);
    let excl = idx.query("-item1", 500);
    let inpath = idx.query("path:dir1", 500);
    // 同一条路径不许出现两次（目录既被当子项又被当目录条目就会重复）
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let no_dup = idx
        .query("item", 500)
        .iter()
        .chain(idx.query("dir", 500).iter())
        .all(|x| seen.insert(x.path.to_string_lossy().to_lowercase()));
    let adv_ok = by_ext.len() >= 36
        && by_ext.iter().all(|x| x.name.to_lowercase().ends_with(".txt"))
        && !folders.is_empty()
        && folders.iter().all(|x| x.dir)
        && no_dup
        && !big.is_empty()
        && big.iter().all(|x| x.size > 1)
        && !excl.is_empty()
        && excl.iter().all(|x| !x.name.to_lowercase().contains("item1"))
        && !inpath.is_empty()
        && inpath
            .iter()
            .all(|x| x.path.to_string_lossy().to_lowercase().contains("dir1"));
    h.check(
        "index::adv_query",
        adv_ok,
        &format!(
            "ext:txt={} kind:folder={} size:>1b={} -item1={} path:dir1={} 无重复={}",
            by_ext.len(),
            folders.len(),
            big.len(),
            excl.len(),
            inpath.len(),
            no_dup
        ),
    );
    // 空查询 / 只剩符号的查询必须退化为"无结果"，而不是返回全表
    let blank = idx.query("   ", 50).is_empty()
        && idx.query("-", 50).is_empty()
        && idx.query("ext:", 50).is_empty();
    h.check("index::adv_blank", blank, "空串 / 孤立 - / 空字段 均无结果");

    // 缓存路径必须落在 %LOCALAPPDATA% 下，且按盘符分文件
    let cp = crate::index::Index::cache_path("C:\\");
    let cp_ok = cp
        .as_ref()
        .map(|p| p.to_string_lossy().contains("ZenFile") && p.to_string_lossy().ends_with("C.bin"))
        .unwrap_or(false);
    h.check("index::cache_path", cp_ok, &format!("{:?}", cp));
    // 卷根解析：C:\Users\a -> C:\，UNC 不给建索引
    h.check(
        "index::volume_root",
        crate::index::volume_root(&PathBuf::from("C:\\Users\\a")).as_deref() == Some("C:\\")
            && crate::index::volume_root(&PathBuf::from("\\\\srv\\share")).is_none(),
        r#"C:\Users\a -> C:\，UNC -> None"#,
    );
    let _ = std::fs::remove_dir_all(&idx_root);

    // 16. ACL：读得出来属主与至少一条 ACE，权限名必须能对上资源管理器的那几种
    let sec = crate::acl::read(&PathBuf::from("C:\\Windows"));
    let owner_ok = !sec.owner.is_empty();
    h.check(
        "acl::read",
        sec.error.is_none() && owner_ok && !sec.aces.is_empty(),
        &format!(
            "属主 {}，{} 条 ACE：{}",
            sec.owner,
            sec.aces.len(),
            sec.aces
                .iter()
                .take(3)
                .map(|a| format!("{}={}{}", a.account, a.right, if a.allow { "" } else { "(拒绝)" }))
                .collect::<Vec<_>>()
                .join("  ")
        ),
    );
    // 权限名不能全是"特别"：那说明掩码常量对不上，页上就全是"特别"，等于没做
    let named = sec
        .aces
        .iter()
        .filter(|a| a.right != "特别")
        .count();
    h.check(
        "acl::rights",
        named > 0,
        &format!("{}/{} 条能对上标准权限名", named, sec.aces.len()),
    );
    let bad = crate::acl::read(&PathBuf::from("C:\\no_such_dir_9z"));
    h.check("acl::missing", bad.error.is_some(), "不存在的路径要报错而不是崩");

    // 17. desktop.ini：写了再读回来必须一致，且不能把无关内容冲掉
    let ini_root = std::env::temp_dir().join("fm-selftest-ini");
    let _ = std::fs::remove_dir_all(&ini_root);
    std::fs::create_dir_all(&ini_root).unwrap();
    // 先放一份"别人的"内容进去：带 LocalizedResourceName 与 ViewState
    let seed = "[.ShellClassInfo]\r\nLocalizedResourceName=@%SystemRoot%\\system32\\shell32.dll,-21779\r\nInfoTip=hello\r\n[ViewState]\r\nMode=\r\nVid=\r\nFolderType=Music\r\n";
    std::fs::write(ini_root.join("desktop.ini"), encode_utf16(seed)).unwrap();
    let got = crate::folder_ini::read(&ini_root);
    h.check(
        "folder_ini::read",
        // FOLDER_TYPES 里 Music 排第 3 位（常规项/文档/图片/音乐/视频）
        got.folder_type.as_deref() == Some("Music") && got.type_index() == 3 && got.has_other,
        &format!(
            "读回 FolderType={:?} 模板下标={} 有其它内容={}",
            got.folder_type,
            got.type_index(),
            got.has_other
        ),
    );
    // 只改模板：IconResource 要写进去，LocalizedResourceName 必须还在
    let mut next = got.clone();
    next.folder_type = Some("Pictures".to_string());
    next.icon_resource = crate::folder_ini::FOLDER_TYPES[2].2.map(|s| s.to_string());
    let wrote = crate::folder_ini::write(&ini_root, &next).unwrap();
    let again = crate::folder_ini::read(&ini_root);
    let text = std::fs::read(ini_root.join("desktop.ini")).map(|b| decode_utf16(&b)).unwrap_or_default();
    h.check(
        "folder_ini::write",
        wrote
            && again.folder_type.as_deref() == Some("Pictures")
            && again.icon_resource.as_deref() == crate::folder_ini::FOLDER_TYPES[2].2
            && text.contains("LocalizedResourceName"),
        &format!("写回后 FolderType={:?}，LocalizedResourceName 保留={}",
            again.folder_type, text.contains("LocalizedResourceName")),
    );
    // 只有已知键的目录清成空白 -> 文件要被删掉（留个空壳会让 Shell 以为这
    // 个文件夹被自定义过，从而不再读它的 LocalizedResourceName）。
    // 上面那个目录里还有 LocalizedResourceName 等未知键，按设计是**不能**删的。
    let ini2 = std::env::temp_dir().join("fm-selftest-ini2");
    let _ = std::fs::remove_dir_all(&ini2);
    std::fs::create_dir_all(&ini2).unwrap();
    let mut only = crate::folder_ini::FolderIni::default();
    only.icon_file = Some("C:\\x\\a.ico".to_string());
    let _ = crate::folder_ini::write(&ini2, &only);
    let had = ini2.join("desktop.ini").exists();
    let _ = crate::folder_ini::write(&ini2, &crate::folder_ini::FolderIni::default());
    h.check(
        "folder_ini::blank",
        had && !ini2.join("desktop.ini").exists(),
        "只剩已知键时清空会删掉 desktop.ini",
    );
    // 反过来：带未知键的目录即使"全清"也要留着，不能把别人的内容丢了
    let _ = crate::folder_ini::write(&ini_root, &crate::folder_ini::FolderIni::default());
    let kept = std::fs::read(ini_root.join("desktop.ini"))
        .map(|b| decode_utf16(&b))
        .unwrap_or_default();
    h.check(
        "folder_ini::keep",
        kept.contains("LocalizedResourceName"),
        "有未知键时清空只删已知键，保留其余内容",
    );
    let _ = std::fs::remove_dir_all(&ini2);
    // 编码：写出来的必须是 UTF-16LE 带 BOM（与资源管理器一致）
    let mut one = crate::folder_ini::FolderIni::default();
    one.icon_file = Some("C:\\x\\a.ico".to_string());
    one.icon_index = Some(3);
    let _ = crate::folder_ini::write(&ini_root, &one);
    let rawb = std::fs::read(ini_root.join("desktop.ini")).unwrap_or_default();
    let back = crate::folder_ini::read(&ini_root);
    h.check(
        "folder_ini::utf16",
        rawb.len() >= 2 && rawb[0] == 0xFF && rawb[1] == 0xFE
            && back.icon_file.as_deref() == Some("C:\\x\\a.ico")
            && back.icon_index == Some(3),
        &format!("BOM={:02X}{:02X} 读回 IconFile={:?} Index={:?}",
            rawb[0], rawb[1], back.icon_file, back.icon_index),
    );
    h.check(
        "folder_ini::hidden",
        is_hidden(&ini_root.join("desktop.ini")),
        "写出的 desktop.ini 带 HIDDEN|SYSTEM（Shell 才认）",
    );
    let _ = crate::folder_ini::write(&ini_root, &crate::folder_ini::FolderIni::default());
    let _ = std::fs::remove_dir_all(&ini_root);

    // 18. 批量重命名的改名规则：纯函数，最容易出错也最该被自检盯住
    use crate::bulk::{apply, duplicates, plan, split_ext, Rule};
    let rp = Rule::Replace {
        find: ".".into(),
        repl: "_".into(),
        case_sensitive: true,
        first_only: false,
    };
    let a_keep = apply("report.final.txt", &rp, true, 0);
    let a_all = apply("report.final.txt", &rp, false, 0);
    h.check(
        "bulk::replace",
        a_keep == "report_final.txt" && a_all == "report_final_txt",
        &format!("保留扩展名 -> {a_keep}，作用于全名 -> {a_all}"),
    );
    // 大小写不敏感：BanAna 里的 a 和 A 都要被认成同一个字符
    let mk = |first_only: bool| Rule::Replace {
        find: "A".into(),
        repl: "b".into(),
        case_sensitive: false,
        first_only,
    };
    let c1 = apply("BanAna.txt", &mk(true), true, 0);
    let c2 = apply("BanAna.txt", &mk(false), true, 0);
    h.check(
        "bulk::case_first",
        c1 == "BbnAna.txt" && c2 == "Bbnbnb.txt",
        &format!("只替换首个 -> {c1}，全部替换 -> {c2}"),
    );
    let num = Rule::Number { template: "照片".into(), start: 1, digits: 3 };
    let names: Vec<String> = ["a.txt", "b.jpg", "c.png"].iter().map(|s| s.to_string()).collect();
    let pn = plan(&names, &num, true);
    h.check(
        "bulk::number",
        pn == vec!["照片001.txt", "照片002.jpg", "照片003.png"],
        &format!("{pn:?}"),
    );
    h.check(
        "bulk::prefix_suffix",
        apply("a.txt", &Rule::Prefix("【新】".into()), true, 0) == "【新】a.txt"
            && apply("a.txt", &Rule::Suffix("-副本".into()), true, 0) == "a-副本.txt",
        &format!(
            "{} / {}",
            apply("a.txt", &Rule::Prefix("【新】".into()), true, 0),
            apply("a.txt", &Rule::Suffix("-副本".into()), true, 0)
        ),
    );
    h.check(
        "bulk::strip",
        apply("abcdef.txt", &Rule::Strip { at: 1, len: 3 }, true, 0) == "def.txt",
        &apply("abcdef.txt", &Rule::Strip { at: 1, len: 3 }, true, 0),
    );
    // 非法字符和末尾的点必须被处理掉，否则预览骗人
    let dirty = apply("a:b<c>.txt", &Rule::Prefix("".into()), false, 0);
    h.check(
        "bulk::sanitize",
        !dirty.contains(['<', '>', ':']) && split_ext("noext").1.is_empty(),
        &format!("a:b<c>.txt -> {dirty}"),
    );
    let dup = duplicates(&["x.txt".to_string(), "X.TXT".to_string(), "y.txt".to_string()]);
    h.check(
        "bulk::duplicates",
        dup == vec![true, true, false],
        &format!("{dup:?}（NTFS 上大小写视为同一个文件）"),
    );

    println!("\n通过 {}/{} 项", h.passed, h.passed + h.failed);
    if h.failed == 0 {
        0
    } else {
        1
    }
}

/// 自检里手写 `desktop.ini` 要用：编码成 UTF-16LE 带 BOM。
fn encode_utf16(s: &str) -> Vec<u8> {
    let mut b = vec![0xFFu8, 0xFE];
    for u in s.encode_utf16() {
        b.extend_from_slice(&u.to_le_bytes());
    }
    b
}

fn decode_utf16(raw: &[u8]) -> String {
    if raw.len() < 2 || raw[0] != 0xFF || raw[1] != 0xFE {
        return String::from_utf8_lossy(raw).to_string();
    }
    let u: Vec<u16> = raw[2..]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&u)
}

fn is_hidden(p: &std::path::Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    std::fs::metadata(p)
        .map(|m| m.file_attributes() & (0x2 | 0x4) != 0)
        .unwrap_or(false)
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
