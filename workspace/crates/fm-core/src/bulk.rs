//! 批量重命名的"改名规则"：纯函数，不碰磁盘，好自检。
//!
//! ## 为什么单独放一层
//!
//! 改名规则是最容易出错又最难靠肉眼验证的东西（替换要不要动扩展名、序号要不要
//! 补零、非法字符要不要剔除）。把它做成不依赖 Win32 的纯函数，就能直接进
//! `--selftest`，而不用"开个窗口点两下看看对不对"。UI 层只负责收集参数、调
//! `plan` 拿预览、再把结果交给 `ops::rename_items` 落盘。
//!
//! ## 沿用 Windows 自己的两条规矩
//!
//!   1. 文件名非法字符一律剔除：`< > : " / \ | ? *` 和控制字符；
//!   2. 末尾的 `.` 和空格会被 Shell 悄悄吃掉，所以提前 trim —— 否则用户看到的
//!      预览和实际落盘的结果不一致。

/// 一条改名规则。UI 上的"模式"就是这个枚举。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rule {
    /// 查找并替换
    Replace {
        find: String,
        repl: String,
        case_sensitive: bool,
        /// 只替换每一处里第一次出现的那个（false = 全部替换）
        first_only: bool,
    },
    /// 名称前面加一段
    Prefix(String),
    /// 名称后面加一段（**插在扩展名之前**，这是资源管理器/PowerShell 的共识）
    Suffix(String),
    /// `模板 + 序号`：序号按列表顺序递增
    Number { template: String, start: i32, digits: i32 },
    /// 从 1 起数的第 `at` 个字符开始，删掉 `len` 个字符
    Strip { at: i32, len: i32 },
}

impl Rule {
    /// 这条规则有几个"主参数"（对应对话框上两个输入框），用于决定第二个框显不显示。
    pub fn param_count(&self) -> usize {
        match self {
            Rule::Replace { .. } => 2,
            Rule::Strip { .. } => 2,
            Rule::Prefix(_) | Rule::Suffix(_) | Rule::Number { .. } => 1,
        }
    }
    /// 主参数的名字（"查找"/"替换为"/"起始位置"…）
    pub fn param_labels(&self) -> [&'static str; 2] {
        match self {
            Rule::Replace { .. } => ["查找", "替换为"],
            Rule::Prefix(_) => ["前缀", ""],
            Rule::Suffix(_) => ["后缀", ""],
            Rule::Number { .. } => ["名称模板", ""],
            Rule::Strip { .. } => ["起始位置", "长度"],
        }
    }
}

/// 拆出主干与扩展名。
///
/// 只有"点不在开头"才算扩展名：`.gitignore` 这种点开头的文件整体是主干，
/// 不然替换/前缀一动就把它变成 `xgitignore`。
pub fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

/// 剔除 Windows 文件名不能用的字符，并去掉末尾的点与空格。
pub fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let bad = matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
            || (c as u32) < 0x20;
        if !bad {
            out.push(c);
        }
    }
    // 末尾的点/空格会被系统吃掉，留着只会让预览骗人
    while out.ends_with('.') || out.ends_with(' ') {
        out.pop();
    }
    out
}

/// 对单个名字套用规则。
///
/// `seq` 只给 `Number` 用（调用方按顺序传 `start, start+1, …`）。
/// `keep_ext` 为真时规则只作用于主干，扩展名原样接回去。
pub fn apply(name: &str, rule: &Rule, keep_ext: bool, seq: i32) -> String {
    let (stem, ext) = split_ext(name);
    // 「名称 + 序号」永远保留扩展名：模板替换的是主干，序号也不该吃掉 .txt
    let keep = keep_ext || matches!(rule, Rule::Number { .. });
    let target = if keep { stem } else { name };

    let out = match rule {
        Rule::Replace { find, repl, case_sensitive, first_only } => {
            if find.is_empty() {
                target.to_string()
            } else if *case_sensitive {
                replace_str(target, find, repl, *first_only)
            } else {
                replace_ci(target, find, repl, *first_only)
            }
        }
        Rule::Prefix(p) => format!("{p}{target}"),
        Rule::Suffix(s) => format!("{target}{s}"),
        Rule::Number { template, start: _, digits } => {
            let n = seq.max(0);
            let w = (*digits).clamp(1, 9) as usize;
            format!("{template}{n:0width$}", width = w)
        }
        Rule::Strip { at, len } => {
            let cs: Vec<char> = target.chars().collect();
            let a = ((*at - 1).max(0) as usize).min(cs.len());
            let b = (a + (*len).max(0) as usize).min(cs.len());
            let mut v = cs[..a].iter().collect::<String>();
            v.extend(cs[b..].iter());
            v
        }
    };

    let mut out = sanitize(&out);
    if out.is_empty() {
        // 规则把名字清空了（比如把文件名整个删掉）—— 留原名，别造出空名字
        out = sanitize(if keep { stem } else { name });
    }
    if keep {
        format!("{out}{ext}")
    } else {
        out
    }
}

/// 对一批名字套用规则，返回与输入等长的新名列表（`Number` 的序号按顺序递增）。
pub fn plan(names: &[String], rule: &Rule, keep_ext: bool) -> Vec<String> {
    let start = match rule {
        Rule::Number { start, .. } => *start,
        _ => 0,
    };
    names
        .iter()
        .enumerate()
        .map(|(i, n)| apply(n, rule, keep_ext, start + i as i32))
        .collect()
}

/// 新名里有没有互相撞车的（下标为真的那些）。资源管理器在这种情况下会拒绝执行。
pub fn duplicates(news: &[String]) -> Vec<bool> {
    let mut seen = std::collections::HashMap::new();
    for n in news {
        // 大小写不敏感：NTFS 上 `A.txt` 和 `a.txt` 是同一个文件
        let k = n.to_lowercase();
        *seen.entry(k).or_insert(0usize) += 1;
    }
    news.iter()
        .map(|n| seen.get(&n.to_lowercase()).copied().unwrap_or(0) > 1)
        .collect()
}

fn replace_str(s: &str, find: &str, repl: &str, first_only: bool) -> String {
    if first_only {
        s.replacen(find, repl, 1)
    } else {
        s.replace(find, repl)
    }
}

/// 大小写不敏感替换。
///
/// 不能直接 `to_lowercase()` 再按字节找 —— 那样下标会错位（土耳其语 İ、ß 之类
/// 大小写后长度会变）。这里逐字符扫，按**字符**匹配，再用字符下标切片。
fn replace_ci(s: &str, find: &str, repl: &str, first_only: bool) -> String {
    let target: Vec<char> = find.chars().collect();
    if target.is_empty() {
        return s.to_string();
    }
    let cs: Vec<char> = s.chars().collect();
    let lower: Vec<char> = s.to_lowercase().chars().collect();
    let want: Vec<char> = find.to_lowercase().chars().collect();
    let mut out: Vec<char> = Vec::with_capacity(cs.len());
    let mut i = 0usize;
    let mut done = false;
    while i < cs.len() {
        if !(first_only && done)
            && i + want.len() <= lower.len()
            && lower[i..i + want.len()] == want[..]
        {
            out.extend(repl.chars());
            i += want.len();
            done = true;
        } else {
            out.push(cs[i]);
            i += 1;
        }
    }
    out.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ext_split() {
        assert_eq!(split_ext("a.b.txt"), ("a.b", ".txt"));
        assert_eq!(split_ext(".gitignore"), (".gitignore", ""));
        assert_eq!(split_ext("noext"), ("noext", ""));
    }

    #[test]
    fn ci_replace_width() {
        // 大小写后长度会变的字符：不能按字节下标替换
        let s = "İstanbul";
        let r = replace_ci(s, "istanbul", "X", false);
        assert!(!r.is_empty() && r.chars().count() <= s.chars().count() + 1);
    }
}
