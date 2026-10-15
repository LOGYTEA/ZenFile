//! 回收站加载基准：把"枚举 / 打开读 $I / 解析"三段时间分开量，
//! 看清楚 4.6 万项到底慢在哪一步。
//!
//! `cargo run --release -p zenfile-core --example recyclebench`

use std::collections::HashMap;
use std::io::Read;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::time::Instant;

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

/// 只枚举：把 $I / $R 的路径收齐，不读任何文件内容。
fn enumerate(root: &Path, sid: &Option<String>) -> (Vec<(Vec<u16>, PathBuf, PathBuf)>, HashMap<Vec<u16>, bool>) {
    let mut i_files: Vec<(Vec<u16>, PathBuf, PathBuf)> = Vec::new();
    let mut r_dirs: HashMap<Vec<u16>, bool> = HashMap::new();
    let Ok(rd) = std::fs::read_dir(root) else { return (i_files, r_dirs) };
    for sid_dir in rd.flatten() {
        if let Some(s) = sid {
            if sid_dir.file_name().to_string_lossy().to_uppercase() != s.to_uppercase() {
                continue;
            }
        }
        let dir = sid_dir.path();
        let Ok(inner) = std::fs::read_dir(&dir) else { continue };
        for e in inner.flatten() {
            let mut wide: Vec<u16> = e.file_name().encode_wide().collect();
            if wide.len() < 3 || wide[0] != '$' as u16 {
                continue;
            }
            let key = wide[2..].to_vec();
            if wide[1] == 'I' as u16 {
                wide[1] = 'R' as u16;
                i_files.push((key, dir.join(std::ffi::OsString::from_wide(&wide)), e.path()));
            } else if wide[1] == 'R' as u16 {
                r_dirs.insert(key, e.file_type().map(|t| t.is_dir()).unwrap_or(false));
            }
        }
    }
    (i_files, r_dirs)
}

fn main() {
    let t_all = Instant::now();
    let t = Instant::now();
    let items = zenfile_core::recycle::list();
    let total = ms(t);
    println!("[全部] {} 项，{:.0} ms（{:.1} 项/ms）", items.len(), total, items.len() as f64 / total);

    // 复现各阶段
    let sid = None; // 拿不到 SID 就用不了，下面用同样的盘根逐个试
    let roots: Vec<PathBuf> = (b'A'..=b'Z')
        .map(|c| PathBuf::from(format!("{}:\\$Recycle.Bin", c as char)))
        .filter(|p| p.exists())
        .collect();
    println!("盘根：{:?}", roots);
    let t = Instant::now();
    let mut all_i: Vec<(Vec<u16>, PathBuf, PathBuf)> = Vec::new();
    let mut all_r: HashMap<Vec<u16>, bool> = HashMap::new();
    for r in &roots {
        let (a, b) = enumerate(r, &sid);
        println!("  {} → $I {} 条 / $R {} 条", r.display(), a.len(), b.len());
        all_i.extend(a);
        all_r.extend(b);
    }
    println!("[阶段1 枚举] {:.0} ms", ms(t));
    let n = all_i.len();
    if n == 0 {
        println!("（$I 为 0，改用 list() 的结果复核，SID 未取到时就这样）");
        println!("总计 {:.0} ms", ms(t_all));
        return;
    }

    // 阶段2：单线程逐条 打开 + 读
    let t = Instant::now();
    let mut got = 0usize;
    let mut buf = [0u8; 4096];
    for (_, _, ip) in all_i.iter().take(n) {
        if let Ok(mut f) = std::fs::File::open(ip) {
            if f.read(&mut buf).is_ok() {
                got += 1;
            }
        }
    }
    let ser = ms(t);
    println!("[阶段2 单线程 打开+读] {got}/{n} 条，{:.0} ms（{:.1} 项/ms）", ser, got as f64 / ser);

    // 拆开看是"打开"慢还是"读"慢
    let t = Instant::now();
    for (_, _, ip) in all_i.iter().take(4000) {
        if let Ok(f) = std::fs::File::open(ip) {
            std::hint::black_box(&f);
        }
    }
    println!("[阶段2a 只打开×4000] {:.0} ms（{:.0} µs/次）", ms(t), ms(t) * 1000.0 / 4000.0);
    let t = Instant::now();
    {
        // 先开着一批句柄，再单独量 read
        let hs: Vec<_> = all_i.iter().take(2000).filter_map(|(_, _, ip)| std::fs::File::open(ip).ok()).collect();
        let t2 = Instant::now();
        let mut b2 = [0u8; 4096];
        for mut f in hs {
            let _ = f.read(&mut b2);
        }
        println!("[阶段2b 只 read×{}] {:.0} ms（{:.0} µs/次）", 2000, ms(t2), ms(t2) * 1000.0 / 2000.0);
    }
    println!("    （上面那一行含开句柄 {:.0} ms）", ms(t));
    // 同一个目录里的普通文件作对照：排除"大目录 + Defender"之外的因素
    let t = Instant::now();
    let tmp = std::env::temp_dir().join("rb-probe");
    let _ = std::fs::create_dir_all(&tmp);
    for i in 0..2000 {
        let p = tmp.join(format!("p{i}.bin"));
        let _ = std::fs::write(&p, b"x".repeat(300));
    }
    println!("[对照 建 2000 个小文件] {:.0} ms", ms(t));
    let t = Instant::now();
    let mut b3 = [0u8; 4096];
    for i in 0..2000 {
        if let Ok(mut f) = std::fs::File::open(tmp.join(format!("p{i}.bin"))) {
            let _ = f.read(&mut b3);
        }
    }
    println!("[对照 打开+读 2000 个] {:.0} ms（{:.0} µs/次）", ms(t), ms(t) * 1000.0 / 2000.0);
    let _ = std::fs::remove_dir_all(&tmp);

    // 阶段3：16 线程
    for threads in [2usize, 4, 8, 16, 32] {
        let t = Instant::now();
        let files = &all_i;
        let chunk = (n / threads).max(1);
        let done: usize = std::thread::scope(|s| {
            (0..n)
                .step_by(chunk)
                .map(|start| {
                    let end = (start + chunk).min(n);
                    s.spawn(move || {
                        let mut c = 0usize;
                        let mut buf = [0u8; 4096];
                        for i in start..end {
                            if let Ok(mut f) = std::fs::File::open(&files[i].2) {
                                if f.read(&mut buf).is_ok() {
                                    c += 1;
                                }
                            }
                        }
                        c
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|h| h.join().unwrap_or(0))
                .sum()
        });
        let d = ms(t);
        println!(
            "[阶段3 {threads:>2} 线程 打开+读] {done}/{n} 条，{:.0} ms（{:.1} 项/ms）",
            d,
            done as f64 / d
        );
    }
    println!("总计 {:.0} ms", ms(t_all));
}
