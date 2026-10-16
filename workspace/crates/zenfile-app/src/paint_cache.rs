//! 第76轮：绘制路径上的**进程级结果缓存**。
//!
//! # 为什么需要它
//!
//! 埋点显示 `panes` 子步骤占 paint 的 48%（p50 2.19ms），但它慢**不是画得慢**，
//! 而是每帧把可见行的纯格式化、每帧问 Shell、每帧读注册表重算一遍。这些结果
//! **只依赖输入、不依赖帧** —— 帧换了而输入没换，结果就该一模一样。
//!
//! # 三条纪律（每条都对应一类真实 bug）
//!
//! 1. **必须有失效路径。** 没有失效的缓存就是 bug：用户改了排序/筛选/列宽之后
//!    画面不更新。本模块统一用「TTL + 有界容量」，调用方**另外**负责在语义上
//!    收紧 key（见各处注释里列出的全部输入）。
//! 2. **不许无界。** 目录规模会变，条目数会涨，所以 `cap` 硬上限 + TTL 清扫。
//! 3. **key 必须包含所有影响结果的输入。** 漏一个就是「改了设置但画面不变」。
//!
//! # 锁的纪律
//!
//! 全部走 `Mutex`，且**毒化（poison）时退化成"缓存未命中"**而不是 panic ——
//! 绘制路径上一帧 panic 会让整个窗口白掉，缓存是纯优化，不值得为它冒险。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 带 TTL 与容量上限的映射。
///
/// 上限策略：插入前先清掉所有过期项；若清理后仍满（说明全是新鲜项），直接
/// **清空重来**。粗暴但有界 —— 走到这一步意味着单个 TTL 窗口内涌入了超过
/// `cap` 个不同 key，实测目录规模下不会发生，真发生了也只是多算一轮，
/// 不会泄漏也不会崩。
pub struct TtlMap<K: Eq + std::hash::Hash, V: Clone> {
    ttl: Duration,
    cap: usize,
    map: HashMap<K, (Instant, V)>,
}

impl<K: Eq + std::hash::Hash, V: Clone> TtlMap<K, V> {
    /// 新建一张缓存表。`ttl` 是过期时长，`cap` 是硬容量上限。
    pub fn new(ttl: Duration, cap: usize) -> Self {
        Self { ttl, cap, map: HashMap::new() }
    }

    /// 取一份**拷贝**。过期或不存在都返回 `None`（调用方走未命中路径）。
    pub fn get(&self, key: &K) -> Option<V> {
        match self.map.get(key) {
            Some((at, v)) if at.elapsed() < self.ttl => Some(v.clone()),
            _ => None,
        }
    }

    /// 写入一条，并顺手维持容量上限。
    pub fn insert(&mut self, key: K, value: V) {
        if self.map.len() >= self.cap {
            self.prune();
            if self.map.len() >= self.cap {
                self.map.clear();
            }
        }
        self.map.insert(key, (Instant::now(), value));
    }

    /// 清扫过期项。O(n)，只在接近容量上限时调用，不在热路径上。
    fn prune(&mut self) {
        let now = Instant::now();
        self.map.retain(|_, (at, _)| now.duration_since(*at) < self.ttl);
    }

    /// 当前条目数（自测与调试用）。
    pub fn len(&self) -> usize {
        self.map.len()
    }
}

/// 线程安全的 `TtlMap` 包装。
///
/// **锁毒化时一律退化成"未命中"**：`get` 返回 `None`、`insert` 变成空操作。
/// 绘制路径宁可多算一次，也不能因为一次 panic 把窗口搞没。
pub struct Cache<K: Eq + std::hash::Hash, V: Clone> {
    inner: Mutex<TtlMap<K, V>>,
}

impl<K: Eq + std::hash::Hash, V: Clone> Cache<K, V> {
    /// 构造一把新锁。`ttl` / `cap` 语义同 [`TtlMap::new`]。
    pub fn new(ttl: Duration, cap: usize) -> Self {
        Self { inner: Mutex::new(TtlMap::new(ttl, cap)) }
    }

    /// 命中则返回值的拷贝，否则 `None`。
    pub fn get(&self, key: &K) -> Option<V> {
        // 每次 draw_item_icons / draw_text 都可能打这里，持锁时间必须短到可忽略，
        // 所以**只在锁内做一次哈希查找**，值是 clone 出来的、不留在锁里。
        self.inner.lock().ok().and_then(|m| m.get(key))
    }

    /// 写入一条缓存。
    pub fn insert(&self, key: K, value: V) {
        if let Ok(mut m) = self.inner.lock() {
            m.insert(key, value);
        }
    }

    /// 当前条目数（自测用）。
    pub fn len(&self) -> usize {
        self.inner.lock().map(|m| m.len()).unwrap_or(0)
    }
}

/// `static` 可直接声明的缓存。
///
/// # 为什么多这一层
///
/// `Cache::new` 不能是 `const fn` —— `HashMap::new()` 不是 const 可调的，
/// 而缓存必须能写成 `static X: LazyCache<..> = LazyCache::new(..)`（调用点零噪音）。
/// `OnceLock` 正好补上这一段：首次 `get`/`insert` 时惰性建表，之后每次只多
/// 一次**已被 CPU 预测住的**原子读。
///
/// 这一层开销只在**未命中**路径上真正发生（建表）；命中路径只多一次
/// `OnceLock::get`，实测在纳秒级，对 2ms 量级的帧预算可以忽略。
pub struct LazyCache<K: Eq + std::hash::Hash, V: Clone> {
    cell: OnceLock<Cache<K, V>>,
    ttl: Duration,
    cap: usize,
}

impl<K: Eq + std::hash::Hash, V: Clone> LazyCache<K, V> {
    /// `const fn`，可直接用在 `static` 上。
    pub const fn new(ttl: Duration, cap: usize) -> Self {
        Self { cell: OnceLock::new(), ttl, cap }
    }

    fn inner(&self) -> &Cache<K, V> {
        self.cell.get_or_init(|| Cache::new(self.ttl, self.cap))
    }

    /// 命中则返回值的拷贝，否则 `None`。
    pub fn get(&self, key: &K) -> Option<V> {
        self.inner().get(key)
    }

    /// 写入一条缓存。
    pub fn insert(&self, key: K, value: V) {
        self.inner().insert(key, value);
    }

    /// 当前条目数（自测用）。
    pub fn len(&self) -> usize {
        self.inner().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_entry_hits_then_expires() {
        // Deterministic on purpose: sleeping for "just past the TTL" races with
        // every other thread in the test binary (this suite runs in parallel and
        // the box is busy), which makes a wall-clock assertion flaky. Instead we
        // (a) assert a fresh entry hits, and (b) assert an entry whose timestamp
        // is older than the TTL is rejected -- by writing that timestamp in
        // directly, no sleeping required.
        let mut m = TtlMap::<&str, i32>::new(Duration::from_millis(40), 8);
        m.insert("k", 7);
        assert_eq!(m.get(&"k"), Some(7), "fresh entry must hit");

        // Backdate the entry well past the TTL.
        let old = Instant::now() - Duration::from_millis(200);
        m.map.insert("k", (old, 7));
        assert_eq!(m.get(&"k"), None, "entry older than TTL must miss");
    }

    #[test]
    fn capacity_is_bounded() {
        let c = Cache::new(Duration::from_secs(60), 16);
        for i in 0..500 {
            c.insert(i, i);
        }
        assert!(c.len() <= 16, "cache grew past cap: {}", c.len());
    }

    #[test]
    fn distinct_keys_do_not_alias() {
        let c = Cache::new(Duration::from_secs(60), 64);
        c.insert(("a", "1"), 10);
        c.insert(("a", "2"), 20);
        c.insert(("b", "1"), 30);
        assert_eq!(c.get(&("a", "1")), Some(10));
        assert_eq!(c.get(&("a", "2")), Some(20));
        assert_eq!(c.get(&("b", "1")), Some(30));
        assert_eq!(c.get(&("b", "2")), None);
    }
}

#[cfg(test)]
mod cost {
    //! 量化 `canonicalize` 的单次真实代价 —— 用它算出改动 1 的**收益上界**。
    //! 侧栏每帧要问 10~30 次，这个数乘出来就是理论天花板。

    use std::time::Instant;

    #[test]
    fn canonicalize_per_call_cost() {
        // 挑一批真实存在的路径，模拟侧栏那 10~30 个目标。
        let targets: Vec<String> = [r"C:\", r"C:\Users", r"C:\Windows", r"C:\Program Files"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        // 预热：第一次调用要付 DLL 惰性初始化，不该算进稳态。
        for t in &targets {
            let _ = std::fs::canonicalize(t);
        }

        let n = 2000;
        let t0 = Instant::now();
        for i in 0..n {
            let _ = std::fs::canonicalize(&targets[i % targets.len()]);
        }
        let per = t0.elapsed().as_secs_f64() / n as f64 * 1e6;
        println!("canonicalize: {:.2} us/call", per);
        println!("  30 calls/frame -> {:.3} ms/frame", per * 30.0 / 1000.0);
        println!("  10 calls/frame -> {:.3} ms/frame", per * 10.0 / 1000.0);
    }
}
