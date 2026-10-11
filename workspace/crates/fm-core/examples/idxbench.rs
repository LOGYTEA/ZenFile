//! 文件名索引基准：回答"为啥实现不了秒搜"。
//!
//! 用法：`cargo run --release -p fm-core --example idxbench -- C:\`
//!
//! 依次测四件事，全程打印毫秒数：
//!   1. 建索引（冷启动，没有缓存时第一次搜索要付的代价）
//!   2. 序列化 + 写盘
//!   3. 反序列化（有缓存时第二次搜索要付的代价）
//!   4. 三种典型查询各跑一遍（短串 / 通配 / 高级语法）

use std::sync::atomic::AtomicBool;
use std::time::Instant;

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

fn main() {
    let root = std::env::args().nth(1).unwrap_or_else(|| "C:\\".to_string());
    println!("=== 文件名索引基准：{root} ===");
    println!(
        "逻辑核 {}",
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
    );

    let cancel = AtomicBool::new(false);

    // 0) 已有缓存的话先报一下大小与条数
    if let Some(p) = fm_core::index::Index::cache_path(&root) {
        if let Ok(md) = std::fs::metadata(&p) {
            println!("\n[0] 现有缓存 {} 字节", md.len());
            let t = Instant::now();
            if let Some(idx) = fm_core::index::Index::load(&root) {
                println!("    载入 {} 条，{:>8.1} ms", idx.len(), ms(t));
            }
        } else {
            println!("\n[0] 无缓存（首次搜索要全卷遍历）");
        }
    }

    // 1) 建索引
    let t = Instant::now();
    let idx = fm_core::index::Index::build(&root, &cancel, Some(&|n: usize| {
        if n % 200_000 < 1000 {
            print!("\r[1] 建索引 ... {n} 条   ");
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }
    }));
    let build_ms = ms(t);
    let n = idx.len();
    println!(
        "\r[1] 建索引 {} 条（complete={}），{:>8.1} ms  → {:.0} 条/秒",
        n,
        idx.complete,
        build_ms,
        n as f64 / (build_ms / 1000.0).max(0.001)
    );

    // 2) 序列化 + 写盘
    let t = Instant::now();
    let bytes = idx.to_bytes();
    let ser_ms = ms(t);
    let t = Instant::now();
    let _ = idx.save();
    let save_ms = ms(t);
    println!(
        "[2] 序列化 {} 字节 {:>7.1} ms；写盘 {:>7.1} ms",
        bytes.len(),
        ser_ms,
        save_ms
    );

    // 3) 反序列化（第二次搜索的真实代价）
    let t = Instant::now();
    let back = fm_core::index::Index::load(&root);
    let load_ms = ms(t);
    println!(
        "[3] 载入缓存 {} 条，{:>8.1} ms",
        back.as_ref().map(|b| b.len()).unwrap_or(0),
        load_ms
    );
    let idx = back.unwrap_or(idx);

    // 4) 查询
    for q in [
        "a",
        "wind",
        "*.dll",
        "system32",
        "ext:log size:>1mb",
        "kind:folder date:>2024-01-01",
        "-temp ext:txt",
    ] {
        let t = Instant::now();
        let hits = idx.query(q, 2000);
        let d = ms(t);
        let first = hits.first().map(|h| h.name.clone()).unwrap_or_default();
        println!("[4] {q:<28} {:>6} 条 {:>7.2} ms   首位={first}", hits.len(), d);
    }

    // 5) 连续击键模拟：`w` → `wi` → `win` → `wind`，每次都全量重查
    print!("[5] 连续击键：");
    let mut acc = 0.0;
    for q in ["w", "wi", "win", "wind", "windo", "window"] {
        let t = Instant::now();
        let h = idx.query(q, 2000);
        let d = ms(t);
        acc += d;
        print!("{q}={d:.1}ms({}) ", h.len());
    }
    println!("\n    合计 {acc:.1} ms");
}
