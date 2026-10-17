//! 领域数据结构与排序策略。
//!
//! 设计约束（技术方案 §5 / §12 内存纪律）：
//!   - 不做递归文件夹大小计算：文件夹 size 恒为 None，按需才可计算；
//!   - 目录树只按需展开，列表不缓存整棵树；
//!   - 缩略图/图标 index 由 UI 层按可见行延迟取，core 只存路径与属性。

use std::path::PathBuf;

use crate::natural::natural_cmp;

/// 单次分批提交的最大条目数（§5 分批提交，避免一次大而全的渲染抖动）。
pub const MAX_BATCH: usize = 256;

/// 枚举代际标识：切换目录即自增，旧批次自然作废（§5 UI 契约）。
pub type Generation = u64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileKind {
    Directory,
    File,
    ReparsePoint,
}

#[derive(Clone, Debug)]
pub struct FileEntry {
    pub name: String,
    pub path: PathBuf,
    pub kind: FileKind,
    /// 文件夹恒为 0 —— 递归大小计算被明确排除（§12）。
    pub size: u64,
    /// FILETIME 折算出的 i64（100ns 步进，自 1601-01-01）
    pub modified: i64,
    /// 创建时间（第33轮："创建日期"列）。同 modified 的编码，0 = 未知。
    #[allow(dead_code)]
    pub created: i64,
    /// 上次访问时间（第33轮："访问日期"列）。0 = 未知（很多文件系统不填）。
    #[allow(dead_code)]
    pub accessed: i64,
    pub attributes: u32,
    pub hidden: bool,
}

impl FileEntry {
    pub fn is_dir(&self) -> bool {
        matches!(self.kind, FileKind::Directory | FileKind::ReparsePoint)
    }

    /// 带隐藏属性的项。
    pub fn is_hidden(&self) -> bool {
        self.hidden
    }

    /// 带系统属性的项（"受保护的操作系统文件"那一类）。
    pub fn is_system(&self) -> bool {
        self.attributes & windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_SYSTEM.0 != 0
    }

    /// 用于列表显示的"类型"描述：取扩展名大写 + 文件，或"文件夹"。
    pub fn type_label(&self) -> String {
        match self.kind {
            FileKind::Directory => "文件夹".to_string(),
            FileKind::ReparsePoint => "链接/重解析点".to_string(),
            FileKind::File => match self.path.extension() {
                Some(e) => format!("{} 文件", e.to_string_lossy().to_uppercase()),
                None => "文件".to_string(),
            },
        }
    }
}

/// 引擎向 UI 提交的一批结果。
#[derive(Clone, Debug)]
pub struct DirBatch {
    pub generation: Generation,
    pub entries: Vec<FileEntry>,
    /// 本次是否为该 generation 的最后一批。
    pub done: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortKey {
    Name,
    Size,
    Modified,
    Type,
}

#[derive(Clone, Copy, Debug)]
pub struct SortSpec {
    pub key: SortKey,
    pub descending: bool,
}

impl Default for SortSpec {
    fn default() -> Self {
        SortSpec { key: SortKey::Name, descending: false }
    }
}

impl SortSpec {
    /// 目录恒定优先（Windows Shell 惯例），其余按 key + 方向。
    pub fn sort(&self, entries: &mut [FileEntry]) {
        entries.sort_by(|a, b| {
            match (a.is_dir(), b.is_dir()) {
                (true, false) => return std::cmp::Ordering::Less,
                (false, true) => return std::cmp::Ordering::Greater,
                _ => {}
            }
            let ord = match self.key {
                SortKey::Name => natural_cmp(&a.name, &b.name),
                SortKey::Size => a.size.cmp(&b.size),
                SortKey::Modified => a.modified.cmp(&b.modified),
                SortKey::Type => a.type_label().cmp(&b.type_label()).then(natural_cmp(&a.name, &b.name)),
            };
            if self.descending {
                ord.reverse()
            } else {
                ord
            }
        });
    }

    pub fn toggle(&mut self, key: SortKey) {
        if self.key == key {
            self.descending = !self.descending;
        } else {
            self.key = key;
            self.descending = false;
        }
    }
}

/// 视图模式。列表/图标共用同一份数据，只改变行矩形与绘制（对应 doc 的多种视图）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ViewMode {
    Details,
    List,
    MediumIcons,
    LargeIcons,
}

impl ViewMode {
    pub fn row_height(&self) -> i32 {
        match self {
            ViewMode::Details | ViewMode::List => 26,
            ViewMode::MediumIcons => 88,
            ViewMode::LargeIcons => 128,
        }
    }

    pub fn next(self) -> ViewMode {
        match self {
            ViewMode::Details => ViewMode::List,
            ViewMode::List => ViewMode::MediumIcons,
            ViewMode::MediumIcons => ViewMode::LargeIcons,
            ViewMode::LargeIcons => ViewMode::Details,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ViewMode::Details => "详细信息",
            ViewMode::List => "列表",
            ViewMode::MediumIcons => "中等图标",
            ViewMode::LargeIcons => "大图标",
        }
    }
}

/// 即时筛选：大小写无关子串匹配。
/// 可见行 = 名称匹配查询 **且** 通过了"隐藏项 / 受保护系统文件"两道开关。
///
/// 两道开关相互独立，和资源管理器的"文件夹选项 → 查看"一致：
/// 系统文件即便勾了"显示隐藏的项目"也仍要单独放行。
pub fn filter_entries(
    entries: &[FileEntry],
    query: &str,
    show_hidden: bool,
    show_system: bool,
) -> Vec<usize> {
    let q = query.to_lowercase();
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            if e.is_system() && !show_system {
                return false;
            }
            if e.is_hidden() && !show_hidden {
                return false;
            }
            q.is_empty() || e.name.to_lowercase().contains(&q)
        })
        .map(|(i, _)| i)
        .collect()
}
