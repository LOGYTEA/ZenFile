# 架构设计

## 目录结构

```
.
├── crates/
│   ├── zenfile-core/     领域层，不依赖任何 UI 框架
│   │   ├── engine        目录枚举（FindFirstFileExW + 大目录取回优化）
│   │   ├── watch         ReadDirectoryChangesW 增量刷新（仅当前可见目录）
│   │   ├── transfer      自研传输引擎（见下）
│   │   ├── index         文件名索引
│   │   ├── clipboard     CF_HDROP 与资源管理器双向互通
│   │   ├── recycle       直接解析 $Recycle.Bin，不经由 COM
│   │   ├── drives        驱动器清单（Win32 直呼，不使用 WMI）
│   │   ├── network       网络驱动器与共享（MPR WNet* 系列）
│   │   ├── bulk          批量重命名规则（纯函数，不访问磁盘）
│   │   ├── plugin/ipc    插件运行时与帧协议
│   │   └── …             config/paths/hotkey/natural/columns/acl/ops…
│   ├── zenfile-app/      Win32 + GDI 自绘界面 → ZenFile.exe
│   └── zenfile-taskmgr/  任务管理模块
```

`zenfile-core` **不依赖 UI 框架是一项硬性约束**，确保更换渲染后端时内核代码无需修改。

## 传输引擎

自研实现，**未使用 `IFileOperation`**。按文件大小分三条路径：

| 文件大小 | 实现路径 | 说明 |
|---|---|---|
| 小于 8 MB | `std::fs::copy` | 内核单流，实测吞吐最优 |
| 8 MB 至 256 MB | 用户态分块并行 | 每个线程复用一个 8 MB 缓冲区 |
| 大于等于 256 MB | `CopyFileExW` + 进度回调 | 转入内核路径，避免用户态双拷贝 |

### 为什么超大文件必须转内核

早期版本把 4.7 GiB 文件切分成 64 块（每块 74 MB），数十个线程并发会产生
GB 级工作集并导致缓存颠簸，CPU 占用饱和而吞吐未同步提升（2.71 CPU-s/GiB）。

对照实测表明，单线程 `std::fs::copy` 相比 32 线程分块**快 45%，CPU 消耗低 3.4 倍**。

::: tip 根本原因
病根是「读入缓冲区再写出」这一**用户态双拷贝路径本身**，而非线程数量不足。
线程越多，只会让缓存颠簸越严重、CPU 越忙。
:::

转入内核路径后的实测结果（4.7 GiB 真实 ISO，跨物理磁盘）：

- 耗时由 7.44 s 降至 **5.78 s**
- CPU 消耗由 1.71 降至 **0.376** CPU-s/GiB
- 峰值线程数由 52 降至 **21**

已追平 Windows 资源管理器（5.89 s）。

### 三个踩过一次的坑

::: danger `PROGRESS_CONTINUE = 0`，`PROGRESS_CANCEL = 1`
写反 = 第一次回调就中止。整个传输直接静默失败。
:::

::: danger `CopyFileEx` 自己建目标文件
所以「预创建目标文件以避免碎片」的循环**必须跳过 `Job::Big`**。
有单测 `kernel_files_are_never_preallocated` 守着这条。
:::

::: danger `Job::Big` 成功后不能走 `bump`
`progress_cb` 已经增量记过进度，再 `bump` 一次 = 速度显示翻倍。
`bump` 因此拆出了 `finish_file`。
:::

另外：取消传输时 `CopyFileEx` 也会失败，**必须先判 cancel 再传播错误**，
否则用户看到的不是「已取消」而是「传输失败」。

## 后台化

删除与批量重命名原先在 UI 线程同步执行，删除 20 000 个文件时窗口直接失去响应。

现已全部迁移至后台线程，结果经由 channel 与 `PostMessage` 回传主线程：

- 删除进行期间主线程延迟 **p50 0.0 ms、p95 4.4 ms**，无卡死
- 期间可以切换目录、切换标签页

::: warning 铁律
**worker 线程绝不碰 UI 状态。** 后台只做纯计算与 I/O，
结果通过 `PostMessage` 回主线程 `drain`。
:::

## 目录扫描

冲突判定原本对每个条目调一次 `Path::exists()`（即 `GetFileAttributesW`），
还得穿过杀软过滤驱动。10 万条目就是 10 万次。

改成 `ExistCache`：按目录缓存 `read_dir` 快照，大小写不敏感对齐 NTFS 语义。

| 阶段 | 10 万条目耗时 |
|---|---:|
| naive（每条目 `Path::exists()`） | 3.07 s |
| **生产实现** | **0.233 s**（13×） |
| 纯枚举下界 | 0.033 s |

::: warning 基准里绝不能重抄一遍生产逻辑
曾经 `bench_dir` 的 engine 臂抄了一份简化版实现，测出「86 倍」，
而生产代码只优化了一半——**优化只落地了一半，数字却是按全量算的**。

现在 benchmark 的 engine 臂直接调 `pub fn plan_only()`，
把「判定结果」和「怎么判定」分开，判错了能一眼看出是哪边。
:::

## 内存管理

- **不递归计算文件夹大小**——属性页用后台线程 + 120ms 增量回传
- **不递归监听目录**——`ReadDirectoryChangesW` 只挂当前可见目录
- 驱动器清单走 Win32 直呼，**不使用 WMI**
- 回收站直接解析 `$Recycle.Bin`，**不经由 COM**

## 子进程与自启动

子进程一律 `CREATE_SUSPENDED` → 入 Job Object → `ResumeThread`，
杜绝「已跑起来才入组失败」的窗口期。

自启动用 `schtasks`，**不用 `HKCU\...\Run`**。

## 剪贴板

`CF_HDROP` + `Preferred DropEffect`，与资源管理器双向互通。

`CF_HDROP` 是双精度 DWORD 偏移表，单位**字节**不是字符，末尾双 NUL。

`GlobalLock` / `GlobalSize` 在 **kernel32**，不在 user32。

::: warning
`OpenClipboard` 必须**重试**——其他程序可能持有。实现里重试 12 次 × 15ms。
另外，粘贴逻辑**不能「源 == 目标就跳过」**，副本后缀是 `X - 副本`。
:::

## 配置

三层，加载顺序为后者覆盖前者，逐键深合并：

| 层级 | 路径 | 优先级 |
|---|---|---|
| 默认 | 编译进二进制的 `defaults.json` | 低 |
| 用户 | `%APPDATA%\ZenFile\settings.json` | 中 |
| 覆盖 | `%APPDATA%\ZenFile\settings.user.json` | **最高** |

图形设置界面**仅写入「用户层」**，不会覆盖「覆盖层」。因此可以放一份
`settings.user.json` 锁定特定偏好，程序在任何情况下都不会将其覆盖。

写入采用原子方式（先写 `.tmp` 再重命名）；配置损坏时自动备份为 `.broken.bak`
并继续使用上层配置。

| 用途 | 位置 |
|---|---|
| 界面布局与菜单条目 | `%APPDATA%\ZenFile\ui_settings.json` |
| 文件名索引缓存 | `%LOCALAPPDATA%\ZenFile\index\{盘符}.bin` |
| 插件 | 可执行文件同级的 `plugins/` |

## 热键

`VK_*` 虚拟键码 + `MOD_NOREPEAT`，**禁 ASCII 字面量**。

::: warning
快捷键处理只挂 `WM_KEYDOWN`。带 Alt 的组合要走 `WM_SYSKEYDOWN`——
漏了会导致**所有 Alt 组合快捷键全部失效**（这个缺陷在性能测试中被发现并修复过）。
:::
