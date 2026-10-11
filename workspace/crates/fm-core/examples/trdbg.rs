use std::os::windows::fs::FileExt;

fn main() {
    let dir = std::env::temp_dir().join("trdbg");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("big.bin");
    let size = 20u64 * 1024 * 1024;
    std::fs::write(&src, vec![0x5Au8; size as usize]).unwrap();
    let n = 5usize;
    let base = size / n as u64;

    for trial in 0..2 {
        let dst = dir.join(format!("out{trial}.bin"));
        let mut hs = Vec::new();
        for k in 0..n {
            let s = src.clone();
            let d = dst.clone();
            hs.push(std::thread::spawn(move || {
                let off = k as u64 * base;
                let len = if k + 1 == n { size - off } else { base };
                let r: std::io::Result<usize> = (|| {
                    let sf = std::fs::File::open(&s).map_err(|e| std::io::Error::other(format!("open src: {e}")))?;
                    if let Some(p) = d.parent() {
                        std::fs::create_dir_all(p)?;
                    }
                    let need = match std::fs::metadata(&d) {
                        Ok(m) => m.len() < size,
                        Err(_) => true,
                    };
                    let df = std::fs::OpenOptions::new()
                        .write(true)
                        .create(true)
                        .truncate(false)
                        .open(&d)
                        .map_err(|e| std::io::Error::other(format!("open dst(need={need}): {e}")))?;
                    if need {
                        df.set_len(size).map_err(|e| std::io::Error::other(format!("set_len: {e}")))?;
                    }
                    let mut buf = vec![0u8; len as usize];
                    let got = sf.seek_read(&mut buf, off).map_err(|e| std::io::Error::other(format!("seek_read: {e}")))?;
                    let mut w = 0usize;
                    while w < got {
                        w += df.seek_write(&buf[w..got], off + w as u64).map_err(|e| std::io::Error::other(format!("seek_write: {e}")))?;
                    }
                    Ok(got)
                })();
                match r {
                    Ok(g) => format!("块 {k} OK 读 {g}"),
                    Err(e) => format!("块 {k} 失败：{e}"),
                }
            }));
        }
        for h in hs {
            println!("  trial{trial} {}", h.join().unwrap());
        }
        let got = std::fs::read(&dst).unwrap_or_default();
        let want = vec![0x5Au8; size as usize];
        println!("  trial{trial} 长度 {} 一致 {}",
                 got.len(),
                 got.len() == want.len() && got == want);
    }
    let _ = std::fs::remove_dir_all(&dir);
}
