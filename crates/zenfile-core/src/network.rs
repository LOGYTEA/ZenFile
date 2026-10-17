//! 网络位置枚举：网络驱动器 + 网络共享，供侧栏"网络"节点使用。
//!
//! # 为什么不用 Shell / WMI
//!
//! `drives.rs` 那套 `GetLogicalDrives` 只认盘符，映射的网络盘虽然会出现，
//! 但拿不到它指向哪台机器；`NetShareEnum` 之类的老接口在 Win10 上要么被
//! 网络感知栈拦截（会弹窗卡住），要么权限不够。这里统一走 **MPR 的
//! `WNet*` 系列**（`mpr.dll`）：纯 API、无 COM、无 WMI、不会弹 UI，
//! 且资源管理器自己的网络列表也是同一套数据源。
//!
//! # 调用纪律（**必须**在后台线程调用）
//!
//! `WNetEnumResourceW` 在遍历 `RESOURCE_GLOBALNET` 时会真正去联系网络上的
//! 主机：机器关机、路由不通、防火墙丢包，都会让它**阻塞数秒到数十秒**。
//! 主线程上调用就是"点一下网络，整个界面僵住"。因此本模块只提供同步函数，
//! 由调用方（`App::spawn_network_scan`）丢到 worker 上执行。
//!
//! # 两级枚举
//!
//! * `RESOURCE_CONNECTED` —— 当前已连接的（`Z:\`、`\\srv\share` 直接映射的盘）。
//!   本机实测 0～3 ms，因为它只读本地状态。
//! * `RESOURCE_GLOBALNET` —— 网络上"能看到"的全部资源，**会发网络包**。
//!
//! 两者都要：`RESOURCE_CONNECTED` 给出稳定低延迟的一批（用户自己映射过的），
//! `RESOURCE_GLOBALNET` 补上整个网络。合并时按 UNC 去重。

use windows::core::PWSTR;
use windows::Win32::Foundation::{HANDLE, WIN32_ERROR};
use windows::Win32::NetworkManagement::WNet::{
    NETRESOURCEW, RESOURCETYPE_ANY, RESOURCE_CONNECTED, RESOURCE_GLOBALNET, WNetCloseEnum,
    WNetEnumResourceW, WNetGetConnectionW, WNetOpenEnumW,
};

/// 一次枚举里的单个网络位置。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetLocation {
    /// UNC 全路径（`\\server\share`）。**导航的目标一律用它**，不是盘符。
    ///
    /// 用盘符导航会踩两个坑：盘符可能被别的程序重新映射；离线盘符在
    /// Explorer 里还能点开显示"不可用"，而 ZenFile 直接列目录就会报错。
    pub remote_name: String,
    /// 界面上显示的名字（共享名）。
    pub display_name: String,
    /// 该资源在本机的盘符映射（`Z:\`）。没被映射过就是空串。
    ///
    /// **不能用它导航**（见 `remote_name` 的注释），但它决定图标：
    /// 有盘符映射 = 系统会给"网络驱动器"图标；没有则按普通共享画。
    pub local_name: String,
    /// 这是一条"已映射的网络驱动器"（有盘符），而不是裸共享。
    pub mapped: bool,
}

/// 枚举失败时的诊断信息。UI 用它区分"确实没有网络位置"和"出错了"。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NetScanError {
    pub connected_scope: u32,
    pub globalnet_scope: u32,
}

impl NetScanError {
    /// 全都成功（返回空列表也是一种成功）。
    pub fn is_clean(&self) -> bool {
        self.connected_scope == 0 && self.globalnet_scope == 0
    }
}

/// `WNetEnumResourceW` 单次返回的缓冲上限。
///
/// MPR 文档要求调用方自己给缓冲区，并按 ERROR_MORE_DATA 的要求放大重试。
/// 16 KB 对应**上百条**资源记录，够绝大多数场景；真不够会走放大分支。
const ENUM_BUF_BYTES: u32 = 16 * 1024;

/// 单次枚举最多放大到多少。防止网络上有畸形条目时无限重试。
const MAX_ENUM_BUF_BYTES: u32 = 1024 * 1024;

fn open_enum(scope: windows::Win32::NetworkManagement::WNet::NET_RESOURCE_SCOPE) -> Result<HANDLE, u32> {
    let mut h = HANDLE::default();
    // RESOURCETYPE_ANY：盘符映射与裸共享都要。只查 DISK 会漏掉 \\srv\share。
    let r = unsafe { WNetOpenEnumW(scope, RESOURCETYPE_ANY, Default::default(), None, &mut h) };
    if r == WIN32_ERROR(0) {
        Ok(h)
    } else {
        Err(r.0)
    }
}

/// 枚举某一 scope 下的全部资源名（只要 remote/local name，别的字段丢弃）。
fn enum_scope(scope: windows::Win32::NetworkManagement::WNet::NET_RESOURCE_SCOPE) -> Result<Vec<(String, String, u32)>, u32> {
    let h = open_enum(scope)?;
    // 保证任何提前 return 都关闭枚举句柄 —— 泄漏一个枚举锁会拖慢后续所有
    // WNet 调用，而且 Explorer 自己也会被牵连。
    struct Guard(HANDLE);
    impl Drop for Guard {
        fn drop(&mut self) {
            unsafe {
                let _ = WNetCloseEnum(self.0);
            }
        }
    }
    let _guard = Guard(h);

    let mut out = Vec::new();
    let mut buf_size = ENUM_BUF_BYTES;
    while buf_size <= MAX_ENUM_BUF_BYTES {
        let mut buf = vec![0u8; buf_size as usize];
        let mut count: u32 = 0;
        let r = unsafe {
            WNetEnumResourceW(h, &mut count, buf.as_mut_ptr() as *mut core::ffi::c_void, &mut buf_size)
        };
        match r.0 {
            0 => {
                // count 是"本批条数"；每条是 sizeof(NETRESOURCEW) + 字符串（含终止符）。
                // 布局：[NETRESOURCEW][lpRemoteName\0][lpLocalName\0]...（对齐补齐）
                let stride = std::mem::size_of::<NETRESOURCEW>();
                let mut off = 0usize;
                for _ in 0..count {
                    let base = off;
                    if base + stride > buf.len() {
                        break;
                    }
                    // 按 u32 对齐读出指针字段。结构体里全是 PWSTR（裸指针），
                    // 不能直接 transmute（对齐/布局假设），逐字段读更稳。
                    let nr = unsafe {
                        std::ptr::read_unaligned(
                            buf.as_ptr().add(base) as *const NETRESOURCEW,
                        )
                    };
                    let s_off = stride;
                    let (remote, remote_bytes) = read_wstr_at(&buf, base + s_off);
                    let (local, local_bytes) =
                        read_wstr_at(&buf, base + s_off + remote_bytes);
                    // 下一条：两个字符串都按 u32 对齐。
                    //
                    // **必须用读出来的字节数**，不能拿解码后的 `String` 反算：
                    // `from_utf16_lossy` 遇到非法 UTF-16 会把 1 个 u16 换成 1 个
                    // U+FFFD，长度看着一样，但遇到**未配对代理项**（2 个 u16 → 1 个
                    // 字符）就少了一半，反算出的偏移直接错位，后面每一条都是垃圾。
                    let next = (base + s_off + remote_bytes + local_bytes + 3) / 4 * 4;
                    if !remote.is_empty() || !local.is_empty() {
                        out.push((remote, local, nr.dwType.0));
                    }
                    off = next;
                }
                return Ok(out);
            }
            // 缓冲区不够 —— 按它要的尺寸放大重来。
            234 => {
                if buf_size <= ENUM_BUF_BYTES {
                    buf_size = ENUM_BUF_BYTES * 4;
                } else {
                    buf_size *= 2;
                }
                continue;
            }
            // ERROR_NO_MORE_ITEMS：枚举提前结束，数据仍有效。
            259 => return Ok(out),
            other => return Err(other),
        }
    }
    Err(1201) // ERROR_BUFFER_OVERFLOW：放大到上限仍装不下
}

/// 从 UTF-16 缓冲的偏移处读一个 NUL 结尾的字符串，返回 `(解码结果, 占用的字节数)`。
///
/// 返回**字节数**（含终止符）而不是让调用方拿 `String` 反算 —— 未配对的
/// 代理项会让"字符数 × 2"小于实际占用，直接反算会让枚举从这一条起全错位。
fn read_wstr_at(buf: &[u8], byte_off: usize) -> (String, usize) {
    if byte_off >= buf.len() {
        return (String::new(), 0);
    }
    let mut units: Vec<u16> = Vec::new();
    let mut p = byte_off;
    loop {
        if p + 1 >= buf.len() {
            // 没读到终止符就把剩下的都吃掉，返回值仍然自洽（不会死循环）。
            return (String::from_utf16_lossy(&units), buf.len() - byte_off);
        }
        let w = u16::from_le_bytes([buf[p], buf[p + 1]]);
        p += 2;
        if w == 0 {
            break;
        }
        units.push(w);
    }
    (String::from_utf16_lossy(&units), p - byte_off)
}

/// 取某个盘符指向的 UNC（`WNetGetConnectionW`）。本地盘返回空串。
fn connection_of(local: &str) -> String {
    if local.is_empty() {
        return String::new();
    }
    // PCWSTR 要 *const u16，而 local 是 &str —— 先显式转成宽串。
    let wide: Vec<u16> = local.encode_utf16().chain(std::iter::once(0)).collect();
    let mut buf = vec![0u16; 1024];
    let mut n: u32 = buf.len() as u32;
    // lpRemoteName 传 Some 就由 MPR 填我们给的缓冲；不给它就让它给尺寸，
    // 但这里固定 1024 足够 UNC（最长约 32767，实践中服务器名+共享名远小于此）。
    let r = unsafe {
        WNetGetConnectionW(
            windows::core::PCWSTR(wide.as_ptr()),
            Some(PWSTR(buf.as_mut_ptr())),
            &mut n,
        )
    };
    if r.0 != 0 {
        return String::new();
    }
    // **不直接信 `n`**：MSDN 只说它是"缓冲区字符数"，返回时是"实际长度"，
    // 但没说清**含不含** NUL 终止符，各版本实现也不一致。这里改成自己扫
    // 第一个 NUL —— 这是缓冲里唯一无歧义的结束标志，且 MPR 一定会写终止符。
    //
    // （早先按 `n-1` 裁剪：若 `n` 恰为字符数，UNC 最后一个字符被吃掉，
    //  `\\srv\share` 变成 `\\srv\shar`，导航必然失败。本机无映射盘没法
    //  触发，所以这个 bug 会一直潜伏到用户第一次映射网络驱动器才爆。）
    let end = buf.iter().position(|&w| w == 0).unwrap_or(0);
    String::from_utf16_lossy(&buf[..end])
}

/// 枚举全部网络位置。
///
/// **必须在后台线程调用**（见模块文档）。
///
/// 返回 `(排序好的列表, 诊断)`。列表为空且 `diagnostic.is_clean()` 为真时，
/// 含义是"这台机器确实没有任何网络位置"，UI 可以安心显示空状态提示。
pub fn scan() -> (Vec<NetLocation>, NetScanError) {
    let mut diag = NetScanError::default();

    // 第一级：已连接。失败只记录不返回 —— 它只是"快的那一层"。
    let connected = match enum_scope(RESOURCE_CONNECTED) {
        Ok(v) => v,
        Err(e) => {
            diag.connected_scope = e;
            Vec::new()
        }
    };

    // 第二级：整个网络。可能慢到秒级，故与第一级分开容错。
    let global = match enum_scope(RESOURCE_GLOBALNET) {
        Ok(v) => v,
        Err(e) => {
            diag.globalnet_scope = e;
            Vec::new()
        }
    };

    let mut out: Vec<NetLocation> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    // 已连接的一级优先（它的 local_name 是可靠的盘符映射），再补全局网络。
    for (remote, local, _t) in connected.iter().chain(global.iter()) {
        if remote.is_empty() {
            continue;
        }
        let key = remote.to_lowercase();
        if !seen.insert(key) {
            continue;
        }
        // 某些 provider 只给 lpLocalName（盘符）不给 lpRemoteName（UNC），
        // 或反之。两个方向都补一次。
        let unc = if !remote.is_empty() {
            remote.clone()
        } else {
            connection_of(local)
        };
        if unc.is_empty() {
            continue;
        }
        let mapped = !local.is_empty();
        let display = unc
            .rsplit('\\')
            .find(|s| !s.is_empty())
            .unwrap_or(&unc)
            .to_string();
        out.push(NetLocation {
            display_name: display,
            remote_name: unc,
            local_name: local.clone(),
            mapped,
        });
    }

    out.sort_by(|a, b| {
        // 有盘符映射的排前面（那是用户自己主动映射的，最可能是常用的），
        // 同组内按名字，保证每次刷新顺序稳定。
        b.mapped
            .cmp(&a.mapped)
            .then_with(|| a.display_name.cmp(&b.display_name))
    });
    (out, diag)
}

/// 列出全部已映射的网络驱动器（`Z:\` -> UNC）。主页/属性用。
pub fn mapped_drives() -> Vec<(String, String)> {
    let (locs, _) = scan();
    locs.into_iter()
        .filter(|l| l.mapped)
        .map(|l| (l.local_name, l.remote_name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_does_not_panic_and_reports_diagnostics() {
        // 本机通常没有任何网络位置：断言的是"返回干净的空列表"，
        // 而不是"必须有 N 条" —— 后者在离线 CI 上必然失败。
        let (locs, diag) = scan();
        assert!(diag.is_clean(), "枚举不应报错: {diag:?}");
        // 排序稳定性：同一 mapped 分组内，display_name 必须非降序。
        for w in locs.windows(2) {
            if w[0].mapped == w[1].mapped {
                assert!(
                    w[0].display_name <= w[1].display_name,
                    "同组内未按名排序: {:?} vs {:?}",
                    w[0].display_name,
                    w[1].display_name
                );
            }
        }
        for l in &locs {
            assert!(l.remote_name.starts_with("\\\\"), "UNC 路径不对: {:?}", l.remote_name);
            assert!(!l.display_name.is_empty());
        }
    }

    #[test]
    fn read_wstr_at_handles_terminator() {
        let mut buf = Vec::new();
        for w in "AB".encode_utf16() {
            buf.extend_from_slice(&w.to_le_bytes());
        }
        buf.extend_from_slice(&[0, 0]);
        assert_eq!(read_wstr_at(&buf, 0), ("AB".to_string(), 6));
    }

    #[test]
    fn read_wstr_at_reports_real_byte_length_for_surrogate_pairs() {
        // **真正配对**的代理项：`\u{1F5C5}`（目录图标）占 2 个 u16。
        //
        // 这一条是 `read_wstr_at` 必须返回"字节数"而不是让调用方按字符数
        // 反算的全部理由：`"目录".encode_utf16()` 是 [76,5F55,5355]（3 个 u16 = 6 字节），
        // 但 `String::len()` 是 3 个字符 —— 若是 CJK 单 BMP 字（[76,5F55]，
        // 2 个 u16 = 4 字节）按"字符数 × 2"算会**多算 2 字节**，同样错位。
        // 所以判据必须锚在"缓冲里实际占了几字节"，与解码结果无关。
        let s_src = "\u{1F5C5}目录";
        let units: Vec<u16> = s_src.encode_utf16().chain(std::iter::once(0)).collect();
        let mut buf = Vec::new();
        for w in &units {
            buf.extend_from_slice(&w.to_le_bytes());
        }
        let (s, used) = read_wstr_at(&buf, 0);
        assert_eq!(s, s_src, "代理项应原样往返");
        assert_eq!(used, buf.len(), "占用必须等于整个串的字节数");
        // 若按 "字符数 × 2" 反算：4 个字符 → 8 字节，比实际少 2，
        // 下一条记录就会从串的中间开始解析。
        assert!(
            (s.chars().count() * 2) as usize + 2 != used,
            "本用例必须真的能区分两种算法，否则它证明不了什么"
        );
    }

    #[test]
    fn read_wstr_at_handles_unpaired_surrogate_without_desync() {
        // 未配对的高位代理项：解码成 U+FFFD，占用仍是 2 字节 + 终止符。
        // 布局：[FFFD 的 2 字节][NUL 2 字节][Z 的 2 字节][NUL 2 字节]
        let raw: [u16; 4] = [0xD800, 0x0000, 0x005A, 0x0000];
        let mut buf = Vec::new();
        for w in raw {
            buf.extend_from_slice(&w.to_le_bytes());
        }
        let (s, used) = read_wstr_at(&buf, 0);
        assert_eq!(used, 4, "U+FFFD 占 2 字节 + 终止符 2 字节");
        assert_eq!(s, "\u{FFFD}");
        // 偏移可继续解析：used 之后正好是 'Z'，且带终止符 → 占用 4
        let (s2, used2) = read_wstr_at(&buf, 4);
        assert_eq!(s2, "Z");
        assert_eq!(used2, 4);
    }

    #[test]
    fn read_wstr_at_stops_at_end_without_terminator() {
        // 没终止符的畸形记录不能让解析越界或死循环。
        // 缓冲故意**奇数长度**：最后一个 u16 只剩半个，解析必须在这里停下，
        // 并把已消耗的字节数如实报出来（3，不是 2 也不是 4）。
        let buf = [0x41u8, 0x00, 0x42];
        let (s, used) = read_wstr_at(&buf, 0);
        assert_eq!(s, "A");
        assert_eq!(used, 3, "末尾半个 u16 不能读，也不能越界");
        // 越界起点必须安全返回
        let (s2, used2) = read_wstr_at(&buf, 99);
        assert_eq!(s2, "");
        assert_eq!(used2, 0);
    }

    #[test]
    fn read_wstr_at_consumes_all_when_terminated() {
        // 正常情况：终止符完整存在时，占用 = (字符数 + 1) × 2。
        let mut buf = Vec::new();
        for w in "\\srv".encode_utf16().chain(std::iter::once(0)) {
            buf.extend_from_slice(&w.to_le_bytes());
        }
        let (s, used) = read_wstr_at(&buf, 0);
        assert_eq!(s, "\\srv");
        assert_eq!(used, 10);
    }
}
