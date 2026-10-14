//! 详细信息视图的**列模型**（第33轮：列头右键自定义）。
//!
//! 语义照抄资源管理器：每个文件夹各存一套"哪些列可见 + 多宽 + 什么顺序"，
//! 存在注册表 bag 里；没去过该目录时用默认值。名字列（Name）**永远第一、
//! 不许隐藏** —— 资源管理器也是这条规矩。
//!
//! 为什么要有这层抽象：以前的画法把"名称/修改日期/类型/大小"四列硬编码在
//! `detail_cols()` 的返回结构里（name/date/kind/size 四个 i32 字段），绘制、
//! 命中、排序、拖宽各写一遍。加一列要动五个地方，隐藏列更没门。这里改成
//! **一个有序的可见列表**，所有几何都由 `layout()` 一次算出来，绘制与命中共用。

/// 一列的稳定 id。**这些字符串会写进配置和 Explorer bag，勿随意改名。**
pub const C_NAME: &str = "name";
pub const C_DATE_MOD: &str = "datemodified";
pub const C_TYPE: &str = "type";
pub const C_SIZE: &str = "size";
pub const C_DATE_CREATED: &str = "datecreated";
pub const C_DATE_ACCESSED: &str = "dateaccessed";
pub const C_AUTHOR: &str = "author";
pub const C_TAGS: &str = "tags";
pub const C_TITLE: &str = "title";
pub const C_PATH: &str = "path";
pub const C_ATTRIBUTES: &str = "attributes";

// —— 第54轮新增（用户要求"选择详细信息"对齐资源管理器的条目量）——
pub const C_EXT: &str = "extension";
pub const C_OWNER: &str = "owner";
pub const C_FILEVERSION: &str = "fileversion";
pub const C_FILEDESCRIPTION: &str = "filedescription";
pub const C_PRODUCTNAME: &str = "productname";
pub const C_COMPANY: &str = "company";
pub const C_COPYRIGHT: &str = "copyright";
pub const C_ALLOC: &str = "allocsize";
pub const C_DOSNAME: &str = "dosname";
pub const C_DIMENSIONS: &str = "dimensions";

/// 表头右键菜单里"选择列…"列出的一组（资源管理器那 8 项 + 我们额外的几项）。
/// 名称列不在可选列表里（不可隐藏），搜索态的"位置"列也不在（那是搜索专用）。
pub const COL_ALL: &[&str] = &[
    C_NAME,
    C_DATE_MOD,
    C_TYPE,
    C_SIZE,
    C_DATE_CREATED,
    C_DATE_ACCESSED,
    C_ATTRIBUTES,
    C_PATH,
    C_EXT,
    C_OWNER,
    C_FILEVERSION,
    C_FILEDESCRIPTION,
    C_PRODUCTNAME,
    C_COMPANY,
    C_COPYRIGHT,
    C_ALLOC,
    C_DOSNAME,
    C_DIMENSIONS,
    C_AUTHOR,
    C_TAGS,
    C_TITLE,
];

/// 各列的显示名（表头文字 / 右键菜单勾选项）。
pub fn label_of(id: &str) -> &'static str {
    match id {
        C_NAME => "名称",
        C_DATE_MOD => "修改日期",
        C_TYPE => "类型",
        C_SIZE => "大小",
        C_DATE_CREATED => "创建日期",
        C_DATE_ACCESSED => "访问日期",
        C_AUTHOR => "作者",
        C_TAGS => "标记",
        C_TITLE => "标题",
        C_PATH => "文件夹路径",
        C_ATTRIBUTES => "属性",
        C_EXT => "扩展名",
        C_OWNER => "所有者",
        C_FILEVERSION => "文件版本",
        C_FILEDESCRIPTION => "文件说明",
        C_PRODUCTNAME => "产品名称",
        C_COMPANY => "公司",
        C_COPYRIGHT => "版权",
        C_ALLOC => "磁盘占用",
        C_DOSNAME => "MS-DOS 名称",
        C_DIMENSIONS => "尺寸",
        _ => "",
    }
}

/// 各列的默认宽度（DIP）。名称最宽，其余按内容长度给。
/// 名称/类型/大小这三列的资源管理器默认值分别是 200/80/80 上下，这里取
/// 与现有默认配置（col_name_w 等）一致的值，见 `App::default_col_width`。
pub fn default_width(id: &str) -> u32 {
    match id {
        C_NAME => 200,
        C_DATE_MOD => 120,
        C_TYPE => 95,
        C_SIZE => 80,
        C_DATE_CREATED => 120,
        C_DATE_ACCESSED => 120,
        C_AUTHOR => 110,
        C_TAGS => 70,
        C_TITLE => 130,
        C_PATH => 220,
        C_ATTRIBUTES => 90,
        C_EXT => 70,
        C_OWNER => 130,
        C_FILEVERSION => 90,
        C_FILEDESCRIPTION => 150,
        C_PRODUCTNAME => 130,
        C_COMPANY => 120,
        C_COPYRIGHT => 150,
        C_ALLOC => 90,
        C_DOSNAME => 130,
        C_DIMENSIONS => 100,
        _ => 90,
    }
}

/// 列宽的合法区间（DIP）。
pub const W_MIN: u32 = 40;
pub const W_MAX: u32 = 600;

/// 某一列的数据从 `FileEntry` 里怎么取。
///
/// 返回 `None` 表示"这个条目没有这项数据"（文件没有作者、目录没有大小…），
/// 调用方画一个 `—`（资源管理器对空值就是这么处理的）。
pub fn cell(id: &str, e: &crate::types::FileEntry, show_ext: bool) -> Option<String> {
    match id {
        C_NAME => {
            let mut s = e.name.clone();
            // 显示扩展名开关：关掉时把 ".xxx" 去掉（与现有行为一致）
            if !show_ext {
                if let Some(dot) = s.rfind('.') {
                    if dot > 0 {
                        s.truncate(dot);
                    }
                }
            }
            Some(s)
        }
        // FILETIME 是"自 1601-01-01 起的 100ns 数"，不是 unix 秒 —— 这里换算
        C_DATE_MOD => fmt_filetime(e.modified),
        C_DATE_CREATED => fmt_filetime(e.created),
        C_DATE_ACCESSED => fmt_filetime(e.accessed),
        C_TYPE => Some(e.type_label()),
        C_SIZE => {
            if e.is_dir() {
                None
            } else {
                // **只给原始字节数**，不带单位 —— fm-app 有按配置选单位
                // （kb/mb/gb/bytes）的 `human_unit`，两边各格式化一次会对不上。
                Some(e.size.to_string())
            }
        }
        C_AUTHOR | C_TAGS | C_TITLE => None,
        C_PATH => Some(
            e.path
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default(),
        ),
        C_ATTRIBUTES => {
            let mut parts: Vec<&str> = Vec::new();
            if e.attributes & windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_READONLY.0 != 0 {
                parts.push("只读");
            }
            if e.is_hidden() {
                parts.push("隐藏");
            }
            if e.is_system() {
                parts.push("系统");
            }
            if e.attributes & windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_ARCHIVE.0 != 0 {
                parts.push("存档");
            }
            if parts.is_empty() {
                None
            } else {
                Some(parts.join(" "))
            }
        }
        // —— 第54轮新增的列。要开句柄 / 读安全描述符 / 解析版本资源的查询
        // **全部走 cached()**：cell() 在绘制与自适应列宽里每行每列都会调，
        // 现查的话一屏几十行就是几十次安全描述符读取，直接把帧率打没。
        C_EXT => {
            let ext = e.path.extension().map(|x| x.to_string_lossy().to_string())?;
            let t = ext.trim().to_string();
            if t.is_empty() { None } else { Some(t) }
        }
        C_OWNER => cached(C_OWNER, &e.path, || owner_of(&e.path)),
        C_FILEVERSION => {
            if e.is_dir() { None } else { cached(C_FILEVERSION, &e.path, || version_string(&e.path, "FileVersion")) }
        }
        C_FILEDESCRIPTION => {
            if e.is_dir() { None } else { cached(C_FILEDESCRIPTION, &e.path, || version_string(&e.path, "FileDescription")) }
        }
        C_PRODUCTNAME => {
            if e.is_dir() { None } else { cached(C_PRODUCTNAME, &e.path, || version_string(&e.path, "ProductName")) }
        }
        C_COMPANY => {
            if e.is_dir() { None } else { cached(C_COMPANY, &e.path, || version_string(&e.path, "CompanyName")) }
        }
        C_COPYRIGHT => {
            if e.is_dir() { None } else { cached(C_COPYRIGHT, &e.path, || version_string(&e.path, "LegalCopyright")) }
        }
        C_ALLOC => {
            if e.is_dir() { None } else { cached(C_ALLOC, &e.path, || alloc_of(&e.path)) }
        }
        C_DOSNAME => cached(C_DOSNAME, &e.path, || dos_of(&e.path)),
        C_DIMENSIONS => {
            if e.is_dir() { None } else { cached(C_DIMENSIONS, &e.path, || dimensions_of(&e.path)) }
        }
        _ => None,
    }
}

// ============================================================ 慢查询与缓存（第54轮）

/// cell() 的进程级缓存：键 = (列 id, 路径)。
///
/// 所有者/版本信息/磁盘占用这类要开句柄或读安全描述符的查询，一次查询几微秒
/// 到几毫秒不等，但绘制每帧每行每列都会来问 —— 不缓存的话只要开了"所有者"列，
/// 一屏 30 行 × 60fps 就是每秒 1800 次安全描述符读取。值的 Option 也一起缓存
/// （查过没有 = 没有，别每帧重查）。超上限整体清空，防长会话内存泄漏。
fn cached(
    col: &str,
    path: &std::path::Path,
    f: impl FnOnce() -> Option<String>,
) -> Option<String> {
    use std::collections::HashMap;
    use std::sync::Mutex;
    static CACHE: Mutex<Option<HashMap<(String, String), Option<String>>>> = Mutex::new(None);
    let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    let key = (col.to_string(), path.to_string_lossy().to_string());
    if let Some(v) = map.get(&key) {
        return v.clone();
    }
    let v = f();
    if map.len() > 20_000 {
        map.clear();
    }
    map.insert(key, v.clone());
    v
}

/// 路径 → 以 NUL 结尾的 UTF-16。
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 文件的所有者（`DOMAIN\用户`）。GetNamedSecurityInfoW 只问 OWNER；
/// 安全描述符**必须 LocalFree**（哪怕只取 owner，SD 也是整个分配出来的）。
fn owner_of(path: &std::path::Path) -> Option<String> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{GetLastError, LocalFree, HLOCAL, WIN32_ERROR};
    use windows::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows::Win32::Security::{
        LookupAccountSidW, OWNER_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, PSID, SID_NAME_USE,
    };
    let wide = to_wide(&path.to_string_lossy());
    unsafe {
        let mut sid = PSID(std::ptr::null_mut());
        let mut sd = PSECURITY_DESCRIPTOR(std::ptr::null_mut());
        let err = GetNamedSecurityInfoW(
            PCWSTR(wide.as_ptr()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            Some(&mut sid),
            None,
            None,
            None,
            &mut sd,
        );
        if err != WIN32_ERROR(0) {
            return None;
        }
        // 两步查询：先拿需要的长度，再取名字（一次给 128 一般够，不够再来一轮）
        let mut name = vec![0u16; 128];
        let mut dom = vec![0u16; 128];
        let (mut cch_name, mut cch_dom) = (128u32, 128u32);
        let mut use_t = SID_NAME_USE::default();
        let ok = LookupAccountSidW(
            PCWSTR::null(),
            sid,
            Some(windows::core::PWSTR(name.as_mut_ptr())),
            &mut cch_name,
            Some(windows::core::PWSTR(dom.as_mut_ptr())),
            &mut cch_dom,
            &mut use_t,
        );
        let _ = LocalFree(Some(HLOCAL(sd.0)));
        if ok.is_err() {
            let e = GetLastError();
            // 长度不够：按系统给的长度重试一轮（错误码 122 = ERROR_INSUFFICIENT_BUFFER）
            if e.0 != 122 {
                return None;
            }
            let mut name = vec![0u16; cch_name as usize];
            let mut dom = vec![0u16; cch_dom as usize];
            let (mut cch_name, mut cch_dom) = (cch_name, cch_dom);
            let mut use_t = SID_NAME_USE::default();
            let ok2 = LookupAccountSidW(
                PCWSTR::null(),
                sid,
                Some(windows::core::PWSTR(name.as_mut_ptr())),
                &mut cch_name,
                Some(windows::core::PWSTR(dom.as_mut_ptr())),
                &mut cch_dom,
                &mut use_t,
            );
            if ok2.is_err() {
                return None;
            }
            return join_owner(&dom[..cch_dom as usize], &name[..cch_name as usize]);
        }
        join_owner(&dom[..cch_dom as usize], &name[..cch_name as usize])
    }
}

/// 域\用户 拼装；域为空就只给用户名。
fn join_owner(dom: &[u16], name: &[u16]) -> Option<String> {
    let cut = |v: &[u16]| -> String {
        let end = v.iter().position(|&c| c == 0).unwrap_or(v.len());
        String::from_utf16_lossy(&v[..end])
    };
    let (d, n) = (cut(dom), cut(name));
    if n.is_empty() {
        return None;
    }
    if d.is_empty() {
        Some(n)
    } else {
        Some(format!("{d}\\{n}"))
    }
}

/// VERSIONINFO 里的一个字符串项（FileVersion / ProductName / …）。
/// 语言代码页先从 \\VarFileInfo\\Translation 拿第一个，再拼子块名查询。
fn version_string(path: &std::path::Path, key: &str) -> Option<String> {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
    };
    let wide = to_wide(&path.to_string_lossy());
    unsafe {
        let mut handle = 0u32;
        let size = GetFileVersionInfoSizeW(PCWSTR(wide.as_ptr()), Some(&mut handle));
        if size == 0 || size > (1 << 20) {
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        GetFileVersionInfoW(PCWSTR(wide.as_ptr()), Some(0), size, buf.as_mut_ptr() as _).ok()?;
        let mut p: *mut std::ffi::c_void = std::ptr::null_mut();
        let mut len = 0u32;
        if !VerQueryValueW(
            buf.as_ptr() as _,
            PCWSTR(to_wide("\\VarFileInfo\\Translation").as_ptr()),
            &mut p,
            &mut len,
        )
        .as_bool()
        {
            return None;
        }
        // Translation 是 {u16 语言, u16 代码页} 的数组；len 是字节数
        let trans = std::slice::from_raw_parts(p as *const u32, (len as usize) / 4);
        if trans.is_empty() {
            return None;
        }
        let (lang, cp) = (trans[0] & 0xFFFF, trans[0] >> 16);
        let sub = to_wide(&format!("\\StringFileInfo\\{lang:04x}{cp:04x}\\{key}"));
        let mut p2: *mut std::ffi::c_void = std::ptr::null_mut();
        let mut len2 = 0u32;
        if !VerQueryValueW(buf.as_ptr() as _, PCWSTR(sub.as_ptr()), &mut p2, &mut len2).as_bool() {
            // 有的文件只有 en-US(040904b0) 一套，翻译表对不上就再试标准英文块
            let sub = to_wide(&format!("\\StringFileInfo\\040904b0\\{key}"));
            if !VerQueryValueW(buf.as_ptr() as _, PCWSTR(sub.as_ptr()), &mut p2, &mut len2)
                .as_bool()
            {
                return None;
            }
        }
        let ws = std::slice::from_raw_parts(p2 as *const u16, len2 as usize);
        let end = ws.iter().position(|&c| c == 0).unwrap_or(ws.len());
        let t = String::from_utf16_lossy(&ws[..end]).trim().to_string();
        if t.is_empty() { None } else { Some(t) }
    }
}

/// 磁盘占用（分配/压缩后的实际字节数）。GetCompressedFileSizeW 不用开句柄。
fn alloc_of(path: &std::path::Path) -> Option<String> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{GetLastError, NO_ERROR};
    use windows::Win32::Storage::FileSystem::GetCompressedFileSizeW;
    let wide = to_wide(&path.to_string_lossy());
    unsafe {
        let mut high = 0u32;
        let low = GetCompressedFileSizeW(PCWSTR(wide.as_ptr()), Some(&mut high));
        let invalid = low == u32::MAX; // INVALID_FILE_SIZE（0.62 没导出常量，值就是 u32::MAX）
        if invalid && GetLastError() != NO_ERROR {
            return None;
        }
        let bytes = ((high as u64) << 32) | low as u64;
        if bytes == 0 {
            return None;
        }
        // **只给原始字节数**，单位由 fm-app 的 human_unit 统一格式化
        Some(bytes.to_string())
    }
}

/// MS-DOS 8.3 短名。卷没开 8.3 名时系统原样返回长路径 —— 那就当没有，
/// 免得这一列和名称长得一模一样。
fn dos_of(path: &std::path::Path) -> Option<String> {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::GetShortPathNameW;
    let long = path.to_string_lossy().to_string();
    let wide = to_wide(&long);
    unsafe {
        // 先探长度（含 NUL），不够再补一轮
        let need = GetShortPathNameW(PCWSTR(wide.as_ptr()), None);
        if need == 0 {
            return None;
        }
        let mut buf = vec![0u16; need as usize];
        let got = GetShortPathNameW(PCWSTR(wide.as_ptr()), Some(&mut buf));
        if got == 0 {
            return None;
        }
        let s = String::from_utf16_lossy(&buf[..got as usize]);
        if s == long || s.is_empty() {
            None
        } else {
            Some(s)
        }
    }
}

/// 图片尺寸（像素）。只解析常见格式的**文件头**（前 64KB 内），不开解码器：
///   PNG（IHDR 定长）、GIF（逻辑屏幕描述符）、BMP（DIB 头）、JPEG（扫 SOF 标记）。
/// 解析不动/不是这些格式 → None（画 `—`，和资源管理器一致）。
fn dimensions_of(path: &std::path::Path) -> Option<String> {
    use std::io::Read;
    let mut head = [0u8; 65_536];
    let n = {
        let mut f = std::fs::File::open(path).ok()?;
        f.read(&mut head).unwrap_or(0)
    };
    let b = &head[..n];
    let (w, h) = if b.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) && n >= 24 {
        // IHDR：宽高各 4 字节大端，紧跟 8 字节签名 + 4 长度 + "IHDR"
        let w = u32::from_be_bytes([b[16], b[17], b[18], b[19]]);
        let h = u32::from_be_bytes([b[20], b[21], b[22], b[23]]);
        (w, h)
    } else if b.starts_with(b"GIF8") && n >= 10 {
        let w = u16::from_le_bytes([b[6], b[7]]) as u32;
        let h = u16::from_le_bytes([b[8], b[9]]) as u32;
        (w, h)
    } else if b.starts_with(b"BM") && n >= 26 {
        let w = i32::from_le_bytes([b[18], b[19], b[20], b[21]]);
        let h = i32::from_le_bytes([b[22], b[23], b[24], b[25]]);
        (w.unsigned_abs(), h.unsigned_abs())
    } else if b.starts_with(&[0xFF, 0xD8]) {
        // JPEG：跳过 0xFFxx 标记段找 SOF0..SOF15（DHT=C4、JPG=C8、DAC=CC 除外）
        let mut i = 2usize;
        let mut hw = None;
        while i + 9 <= n {
            if b[i] != 0xFF {
                break;
            }
            let marker = b[i + 1];
            if (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xC8 && marker != 0xCC
            {
                let h = u16::from_be_bytes([b[i + 5], b[i + 6]]) as u32;
                let w = u16::from_be_bytes([b[i + 7], b[i + 8]]) as u32;
                hw = Some((w, h));
                break;
            }
            let seg = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
            if seg < 2 {
                break;
            }
            i += 2 + seg;
        }
        hw?
    } else {
        return None;
    };
    if w == 0 || h == 0 || w > 1_000_000 || h > 1_000_000 {
        return None;
    }
    Some(format!("{w} × {h}"))
}

/// FILETIME（100ns since 1601-01-01）→ `YYYY/MM/DD HH:MM` 本地时间。
/// 0 = 未知 → None（画 `—`）。
fn fmt_filetime(ft: i64) -> Option<String> {
    // 1601-01-01 到 1970-01-01 的秒差
    const EPOCH_DIFF: i64 = 11_644_473_600;
    if ft <= 0 {
        return None;
    }
    let secs = ft / 10_000_000 - EPOCH_DIFF;
    if secs < 0 {
        return None;
    }
    let local = secs + local_offset_secs(secs);
    let (y, m, d, hh, mm) = civil_from_unix(local);
    Some(format!("{y:04}/{m:02}/{d:02} {hh:02}:{mm:02}"))
}

/// 本地时区偏移（秒），按**该时刻**判定（含夏令时）。
///
/// Windows 的换算是 `UTC = 本地 + Bias*60`，Bias 单位是**分钟**，
/// 所以 `本地 = UTC - Bias*60`，偏移秒数 = `-Bias*60`；夏令时期间
/// 生效的是 `Bias + DaylightBias`。
///
/// 第39轮审计 #7 修的三处错（东八区实测差 8 小时 8 分）：
///   1. **分钟当秒用**：Bias=-480 被当成 -480 秒；
///   2. **符号反了**：应是 `utc - Bias*60`，旧版直接 `utc + Bias`；
///   3. **完全不看夏令时**：`GetTimeZoneInformation` 的返回值只说明
///      "现在"在不在夏令时，而文件的修改日期可能是去年冬天的 ——
///      必须拿该时刻自己去比 `DaylightDate`→`StandardDate` 窗口。
fn local_offset_secs(unix: i64) -> i64 {
    use windows::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
    let mut tz = TIME_ZONE_INFORMATION::default();
    unsafe {
        GetTimeZoneInformation(&mut tz);
    }
    let bias = if in_dst(&tz, unix) {
        tz.Bias + tz.DaylightBias
    } else {
        tz.Bias
    };
    -(bias as i64) * 60
}

/// 该 UTC 秒是否落在本时区的夏令时窗口。
///
/// 窗口由 `DaylightDate` → `StandardDate` 给出（SYSTEMTIME：月份 + "第几个
/// 星期几"）。日期列只到分钟，切换当天差一两小时无感，所以按**日**比较；
/// 旧版干脆不分 DST，那才是小时级的错。
fn in_dst(tz: &windows::Win32::System::Time::TIME_ZONE_INFORMATION, unix: i64) -> bool {
    if tz.DaylightBias == 0 {
        return false; // 没配夏令时（中国大陆：Bias=-480、DaylightBias=0）
    }
    // 先按**标准时间**折算出本地年月日：否则要先知道偏移才知道有没有 DST，
    // 是个环；窗口边界差几小时对日期列无影响。
    let local_std = unix - (tz.Bias as i64) * 60;
    let (y, m, d, _, _) = civil_from_unix(local_std);
    let cur = (m, d);
    let Some(dst) = nth_weekday(y, tz.DaylightDate) else { return false };
    let Some(std) = nth_weekday(y, tz.StandardDate) else { return false };
    if dst <= std {
        // 北半球：3 月进、11 月出
        cur >= dst && cur < std
    } else {
        // 南半球：10 月进、次年 4 月出（窗口跨年）
        cur >= dst || cur < std
    }
}

/// SYSTEMTIME（wMonth + wDay=第几个星期几, 5=最后一个 + wDayOfWeek）
/// → 该年的 (月, 日)。wMonth==0 表示"这个时区没有这档切换"，返回 None。
fn nth_weekday(year: i64, st: windows::Win32::Foundation::SYSTEMTIME) -> Option<(u32, u32)> {
    let month = st.wMonth as u32;
    if month == 0 || month > 12 {
        return None;
    }
    let week = (st.wDay as u32).clamp(1, 5);
    let want_dow = (st.wDayOfWeek as u32) % 7; // 0=周日
    // 该月 1 号星期几（1970-01-01 是周四=4）
    let dow1 = ((days_from_civil(year, month, 1) + 4).rem_euclid(7)) as u32;
    let first = 1 + (want_dow + 7 - dow1) % 7; // 该月第一次出现，落在 1..7
    let dim = if month == 12 {
        (days_from_civil(year + 1, 1, 1) - days_from_civil(year, 12, 1)) as u32
    } else {
        (days_from_civil(year, month + 1, 1) - days_from_civil(year, month, 1)) as u32
    };
    let mut day = first + (week - 1) * 7;
    if day > dim {
        day -= 7; // wDay=5 但这个月只出现 4 次 → 落到第 4 次
    }
    Some((month, day))
}

/// `civil_from_unix` 的逆：年月日 → 距 1970-01-01 的天数（Howard Hinnant）。
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m as i64 - 3 } else { m as i64 + 9 };
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Unix 秒 → (年, 月, 日, 时, 分)。Howard Hinnant 的 civil_from_days 算法。
fn civil_from_unix(secs: i64) -> (i64, u32, u32, u32, u32) {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let hh = (rem / 3600) as u32;
    let mm = ((rem % 3600) / 60) as u32;
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as i64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, hh, mm)
}

/// 把配置里的 `visible` 列表**洗成**一个合法的显示序列。
///
/// 规则（与资源管理器一致）：
///   - 名称列恒在第一位，且永不可去掉
///   - 只认 `COL_ALL` 里的 id，脏值丢弃（手改配置也不会把界面搞坏）
///   - 重复的只留第一次
///   - 空列表 → 返回默认四列
pub fn normalize_visible(raw: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(raw.len().max(4));
    out.push(C_NAME.to_string());
    for id in raw {
        if id == C_NAME || out.iter().any(|x| x == id) {
            continue;
        }
        if COL_ALL.contains(&id.as_str()) {
            out.push(id.clone());
        }
    }
    if out.len() == 1 {
        // 全被禁掉 = 只剩名称 → 补回默认的其余三列（否则界面根本没法用）
        for d in [C_DATE_MOD, C_TYPE, C_SIZE] {
            if !out.iter().any(|x| x == d) {
                out.push(d.to_string());
            }
        }
    }
    out
}

/// 默认显示的四列。
pub fn default_visible() -> Vec<String> {
    vec![
        C_NAME.to_string(),
        C_DATE_MOD.to_string(),
        C_TYPE.to_string(),
        C_SIZE.to_string(),
    ]
}

#[cfg(test)]
mod slow_query_tests {
    use super::*;

    #[test]
    fn dimensions_of_png_header() {
        let mut b: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];
        b.extend_from_slice(b"IHDR");
        b.extend_from_slice(&137u32.to_be_bytes());
        b.extend_from_slice(&50u32.to_be_bytes());
        b.extend_from_slice(&[8, 6, 0, 0, 0]);
        let p = std::env::temp_dir().join("zenfile_dim_test.png");
        std::fs::write(&p, &b).unwrap();
        assert_eq!(dimensions_of(&p).as_deref(), Some("137 × 50"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn dimensions_of_gif_bmp_jpeg() {
        let dir = std::env::temp_dir();
        // GIF：逻辑屏幕描述符在 6..10，小端
        let gif = [b'G', b'I', b'F', b'8', b'9', b'a', 0x89, 0x00, 0x32, 0x00];
        let pg = dir.join("zenfile_dim_test.gif");
        std::fs::write(&pg, gif).unwrap();
        assert_eq!(dimensions_of(&pg).as_deref(), Some("137 × 50"));
        // BMP：DIB 头宽高在 18..26，小端 i32
        let mut bmp = vec![b'B', b'M'];
        bmp.extend_from_slice(&[0u8; 16]);
        bmp.extend_from_slice(&137i32.to_le_bytes());
        bmp.extend_from_slice(&50i32.to_le_bytes());
        let pb = dir.join("zenfile_dim_test.bmp");
        std::fs::write(&pb, &bmp).unwrap();
        assert_eq!(dimensions_of(&pb).as_deref(), Some("137 × 50"));
        // JPEG：SOF0 段里高前宽后（FF C0 len prec 高2 宽2）
        let jpg = [0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08, 0x00, 0x32, 0x00, 0x89, 0x03];
        let pj = dir.join("zenfile_dim_test.jpg");
        std::fs::write(&pj, jpg).unwrap();
        assert_eq!(dimensions_of(&pj).as_deref(), Some("137 × 50"));
        let _ = (std::fs::remove_file(&pg), std::fs::remove_file(&pb), std::fs::remove_file(&pj));
    }

    #[test]
    fn dimensions_of_txt_is_none() {
        let p = std::env::temp_dir().join("zenfile_dim_test.txt");
        std::fs::write(&p, b"hello").unwrap();
        assert_eq!(dimensions_of(&p), None);
        let _ = std::fs::remove_file(&p);
    }

    /// 测试进程自己的可执行文件是一个真实存在的文件：所有者必查得到，
    /// 磁盘占用必 ≥ 文件大小（簇对齐只会变大）。
    #[test]
    fn owner_and_alloc_of_real_file() {
        let exe = std::env::current_exe().unwrap();
        let owner = owner_of(&exe);
        assert!(owner.is_some(), "连自己的 exe 都查不到所有者");
        assert!(!owner.unwrap().is_empty());
        let alloc = alloc_of(&exe).map(|s| s.parse::<u64>().unwrap()).unwrap();
        let size = std::fs::metadata(&exe).unwrap().len();
        assert!(alloc >= size, "磁盘占用 {alloc} 小于文件大小 {size}");
    }

    /// C: 盘 8.3 名通常开着 → 短名应查得到且不等于长名；查不到也不能崩。
    #[test]
    fn dos_of_real_file() {
        let exe = std::env::current_exe().unwrap();
        if let Some(s) = dos_of(&exe) {
            assert_ne!(s, exe.to_string_lossy().to_string());
            assert!(s.to_lowercase().ends_with(".exe"));
        }
    }

    #[test]
    fn new_columns_in_col_all_and_labels() {
        for id in [
            C_EXT, C_OWNER, C_FILEVERSION, C_FILEDESCRIPTION, C_PRODUCTNAME, C_COMPANY,
            C_COPYRIGHT, C_ALLOC, C_DOSNAME, C_DIMENSIONS,
        ] {
            assert!(COL_ALL.contains(&id), "{id} 不在 COL_ALL");
            assert!(!label_of(id).is_empty(), "{id} 没有显示名");
            assert!(default_width(id) >= W_MIN && default_width(id) <= W_MAX);
        }
        // 名称仍然恒在 COL_ALL 首位（对话框与 normalize 都依赖这个顺序）
        assert_eq!(COL_ALL[0], C_NAME);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_keeps_name_first() {
        let v = normalize_visible(&["size".into(), "name".into(), "type".into()]);
        assert_eq!(v, vec!["name", "size", "type"]);
    }

    #[test]
    fn normalize_drops_unknown_and_dupes() {
        let v = normalize_visible(&["size".into(), "bogus".into(), "size".into()]);
        assert_eq!(v, vec!["name", "size"]);
    }

    #[test]
    fn normalize_empty_falls_back_to_defaults() {
        assert_eq!(normalize_visible(&[]), default_visible());
    }

    #[test]
    fn civil_from_unix_epoch() {
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0));
    }

    /// 权威对照：Win32 自己的 FileTimeToLocalFileTime。
    ///
    /// 旧版（分钟当秒 + 符号反 + 无 DST）在东八区这条断言下差 8 小时 8 分。
    /// 冬夏各取一个时刻，覆盖 DST 的两个态（本机没配 DST 时两态相同，
    /// 但只要时区带 DST，这条就会走到 Bias+DaylightBias 分支）。
    #[test]
    fn fmt_filetime_matches_win32_local_time() {
        use windows::Win32::Foundation::FILETIME;
        use windows::Win32::Storage::FileSystem::FileTimeToLocalFileTime;
        const EPOCH_DIFF: i64 = 11_644_473_600;
        // 2025-12-01 / 2026-06-15 / 2026-08-02（UTC）
        for unix in [1_764_547_200i64, 1_781_500_800, 1_785_628_800] {
            let ticks = (unix + EPOCH_DIFF) * 10_000_000;
            let ft = FILETIME {
                dwLowDateTime: ticks as u32,
                dwHighDateTime: (ticks >> 32) as u32,
            };
            let mut lft = FILETIME::default();
            unsafe { FileTimeToLocalFileTime(&ft, &mut lft).unwrap() };
            let lticks =
                ((lft.dwHighDateTime as i64) << 32) | lft.dwLowDateTime as i64;
            let (y, m, d, hh, mm) = civil_from_unix(lticks / 10_000_000 - EPOCH_DIFF);
            let want = format!("{y:04}/{m:02}/{d:02} {hh:02}:{mm:02}");
            assert_eq!(
                fmt_filetime(ticks).as_deref(),
                Some(want.as_str()),
                "unix={unix}"
            );
        }
    }

    /// 偏移必须是**合法时区秒数**（±14h 内、整分钟）。
    /// 旧版把分钟当秒：东八区量出 -480 秒（8 分钟），一眼就不对。
    #[test]
    fn offset_is_plausible_timezone_seconds() {
        let off = local_offset_secs(1_764_547_200);
        assert!(
            (-14 * 3600..=14 * 3600).contains(&off),
            "偏移 {off}s 不是合法时区（分钟当秒的典型症状是 ±800 内的小数字）"
        );
        assert_eq!(off % 60, 0, "时区偏移必须整分钟");
    }

    #[test]
    fn civil_from_unix_known_date() {
        // 2020-01-01T00:00:00Z = 1577836800
        assert_eq!(civil_from_unix(1_577_836_800), (2020, 1, 1, 0, 0));
    }
}
// ============================================================ 缓存文件夹（第33轮）

/// 这些名字（不区分大小写）**整个目录**都是缓存/临时产物。
///
/// 只看**目录自己的名字**，不看父路径 —— 用户看到 `C:\Users\x\AppData\Local\Temp`
/// 下的 `Temp` 时，要的是"这个文件夹是临时的"这个判断，而不是因为它碰巧在
/// AppData 下面就整条路径都变灰（那会让 Local\Packages 之类正常目录也变灰）。
const CACHE_NAMES: &[&str] = &[
    "temp", "tmp", "cache", "caches", "cache2", "cachedata", "log", "logs",
    "crashdumps", "dumps", "gpucache", "code cache", "shadercache", "dawncache",
    "webcache", "cache_storage", ".cache", ".tmp", "thumbs.db",
];

/// 目录名**以**这些片段结尾的也算缓存（`node_modules/.cache2`、`x.tmp` 之类）。
const CACHE_SUFFIXES: &[&str] = &[".cache", ".tmp", ".temp", ".log", ".logs"];

/// 这个文件夹是不是"缓存/临时"性质。
///
/// 命中条件（任一）：
///   1. 目录名去扩展名后与 `CACHE_NAMES` 某项完全相同
///   2. 目录名以 `CACHE_SUFFIXES` 某项结尾
///
/// **只在 `is_dir` 为真时**才可能为 true（调用方负责）。
pub fn is_cache_dir(name: &str) -> bool {
    let n = name.trim().to_lowercase();
    if n.is_empty() {
        return false;
    }
    // 先剥掉一层扩展名：`foo.tmp` / `cache.dir` 之类
    let stem = match n.rfind('.') {
        Some(i) if i > 0 => &n[..i],
        _ => n.as_str(),
    };
    if CACHE_NAMES.iter().any(|c| *c == n || *c == stem) {
        return true;
    }
    CACHE_SUFFIXES.iter().any(|s| n.ends_with(s))
}

#[cfg(test)]
mod cache_tests {
    use super::is_cache_dir;

    #[test]
    fn exact_names() {
        assert!(is_cache_dir("Temp"));
        assert!(is_cache_dir("TEMP"));
        assert!(is_cache_dir("cache"));
        assert!(is_cache_dir(".cache"));
        assert!(is_cache_dir("Code Cache"));
        assert!(is_cache_dir("GPUCache"));
        assert!(is_cache_dir("CrashDumps"));
    }

    #[test]
    fn suffix_names() {
        assert!(is_cache_dir("build.tmp"));
        assert!(is_cache_dir("x.cache"));
        assert!(is_cache_dir("app.log"));
    }

    #[test]
    fn normal_dirs_not_cache() {
        assert!(!is_cache_dir("Program Files"));
        assert!(!is_cache_dir("Windows"));
        assert!(!is_cache_dir("TempFiles"));   // 不完全相同
        assert!(!is_cache_dir("Cacheable"));   // 不含后缀
        assert!(!is_cache_dir("Templates"));
    }
}
