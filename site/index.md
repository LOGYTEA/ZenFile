---
layout: home

hero:
  name: ZenFile
  text: 原生 Win32 文件管理器
  tagline: 轻量 · 原生 · 高性能 · 可拓展
  image:
    src: /images/home.png
    alt: ZenFile 主界面
  actions:
    - theme: brand
      text: 开始使用
      link: /guide/install
    - theme: alt
      text: 开发文档
      link: /docs/new-plugin

features:
  - title: 真正的原生
    details: Win32 消息循环 + GDI 自绘，不依赖 Electron、Qt、WebView2、Node.js 或 .NET 运行时。单文件可执行文件 1.48 MiB。
  - title: 1.48 MiB
    details: 主程序体积。空闲常驻内存 29.5 MB，约为 Windows 资源管理器的一半。
  - title: 自绘传输引擎
    details: 不用 IFileOperation。按文件大小分三条路径，4.7 GiB 大文件 5.78 s，已追平资源管理器。
  - title: 进程级插件
    details: 插件是 ZenFile.exe --plugin <id> 拉起的独立进程。崩溃不带崩宿主，Job Object 兜底。
  - title: 完全可配置
    details: 8 个设置页：界面布局、外观、交互、右键菜单、快捷键、高级、插件、关于。
  - title: 全量实测数据
    details: 与资源管理器 / Pulse 的对比全部为本机实测，每项写明口径与判据，无厂商宣传数字。
---

<div class="hero-mark">
<svg viewBox="0 0 96 96" fill="none" xmlns="http://www.w3.org/2000/svg">
  <path d="M8 26a8 8 0 0 1 8-8h20l8 8h36a8 8 0 0 1 8 8v42a8 8 0 0 1-8 8H16a8 8 0 0 1-8-8V26Z" fill="#CE7543"/>
  <path d="M8 40h80" stroke="#B3612F" stroke-width="3"/>
</svg>
</div>

## 它是什么

ZenFile 是一个面向 Windows 10/11 x64 的原生文件管理器。界面上的右键菜单、属性页、
进度窗口、设置面板全部是自绘的像素，没有一处来自系统控件的默认皮肤。

项目受 [LiteLoaderQQNT](https://github.com/LiteLoaderQQNT/LiteLoaderQQNT) 启发，
设计取向是：**原生程序同样可以兼顾资源占用与界面表现力**——不必在「快」和「好看」之间二选一。

![详细视图](/images/detail.png)

## 核心数据

全部来自本机实测（Windows 10 LTSC 21H2 / Ryzen 7 7435H / 16 GB / 2×NVMe），
采集方法与口径见[性能实测](/docs/performance)。

| 测试项目 | ZenFile | 资源管理器 | Pulse |
|---|---:|---:|---:|
| 空闲内存（RSS） | **29.5 MB** | 54.7 MB | 214.5 MB |
| 10 000 个小文件操作 | **5 109 文件/秒** | 672 | 1 039 |
| 单个 4.7 GiB ISO 复制 | 5.78 s | 5.89 s | **4.23 s** |
| 10 万文件目录加载 | **0.481 s** | 0.620 s | 0.967 s |
| 删除 2 万文件期间 UI 延迟 | **p50 0.0 ms，无卡死** | — | — |

## 三句话说明它怎么做到

**目录扫描靠缓存，不靠反复试探。** 冲突判定原本对每个条目调一次 `Path::exists()`，
十万条目就是十万次 `GetFileAttributesW`——还要穿过杀软过滤驱动。改成按目录缓存
`read_dir` 快照（大小写不敏感对齐 NTFS）后，10 万条目从 3.07 s 降到 0.233 s（13×）。

**大文件不并行，走内核。** 早期把 4.7 GiB 切成 64 块、数十线程并发，结果是 GB 级工作集、
缓存颠簸、CPU 饱和而吞吐没涨。实测发现单线程 `std::fs::copy` 比 32 线程分块**快 45%、
CPU 少 3.4 倍**——病根是「读进用户态缓冲再写出」的双拷贝，不是线程不够。
≥256 MB 直接转 `CopyFileExW` + 进度回调。

**删除和批量重命名全在后台。** 原先在 UI 线程同步跑，删 2 万个文件窗口直接失去响应。
现在全部走后台线程 + channel 回传，删除期间主线程延迟 p50 0.0 ms、p95 4.4 ms，
并且可以正常切换目录。

## 许可

Apache-2.0。受 LiteLoaderQQNT 启发。
