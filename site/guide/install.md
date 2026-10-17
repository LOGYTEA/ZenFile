# 安装

ZenFile 提供安装版与便携版两种分发形态，均位于 [Releases](https://github.com/LOGYTEA/ZenFile/releases)
的 Assets 目录。

## 安装版

`ZenFile-setup-<版本号>.exe` 是单文件自解压安装包。双击执行后默认安装到

```
%LOCALAPPDATA%\Programs\ZenFile
```

并创建桌面与开始菜单快捷方式。

需要装到别的位置，可以直接把路径作为参数传给它：

```bash
ZenFile-setup-0.3-Beta-Weekly26-10-17.exe "D:\Applications\ZenFile"
```

::: tip 卸载
安装版不做注册表登记，卸载即删除安装目录。配置写在 `%APPDATA%\ZenFile\`，
需要彻底清理时一并删除该目录。
:::

## 便携版

`ZenFile-portable-<版本号>.zip` 解压即用，无需安装。解压后的目录结构：

```
ZenFile-portable-0.3-Beta/
├── ZenFile.exe
├── plugins/           ← 插件放这里（不是 %APPDATA%）
└── ...
```

配置写入 `%APPDATA%\ZenFile\`，插件目录在可执行文件**同级**。
因此解压目录可以整体复制或移动，绿色便携运行。

::: warning 插件目录的位置
插件目录必须和 `ZenFile.exe` 放在一起，不是 exe 的工作目录。
放错位置「设置 - 插件」面板会显示「没有找到插件」。
:::

## 系统要求

| 项 | 要求 |
|---|---|
| 操作系统 | Windows 10 / 11 x64 |
| 运行时 | **无需任何运行时** |
| 内存 | 空闲常驻 29.5 MB |

不依赖 WebView2、Node.js、.NET、Visual C++ 运行库。

## 命令行参数

| 参数 | 说明 |
|---|---|
| `--selftest` | 执行无界面自检，全部通过时返回 0 |
| `--version` | 输出版本号与协议版本 |
| `--tray` | 以托盘模式启动 |
| `--new-process` | 强制创建新进程，不占用单实例互斥量 |
| `--plugin <id>` | 以插件进程身份启动，由宿主调用 |
| `--anim-trace <ms>` | 帧链路埋点记录到 `%TEMP%` 下的 CSV |

## 从源码构建

```bash
git clone https://github.com/LOGYTEA/ZenFile.git
cd ZenFile
cargo build --release        # 产出 target/release/ZenFile.exe
cargo test -p zenfile-core   # 内核单元测试
```

需要 Rust 1.85 及以上（msvc 目标）。构建出的可执行文件同样是单文件、无外部依赖。

## 与资源管理器互通

ZenFile 使用系统标准的 `CF_HDROP` + `Preferred DropEffect` 剪贴板格式，
与 Windows 资源管理器**双向互通**：

- 在 ZenFile 里复制的文件，到资源管理器里可以直接 `Ctrl+V` 粘贴
- 在资源管理器里复制的文件，到 ZenFile 里 `Ctrl+V` 同样有效
- 支持跨 ZenFile 实例复制粘贴

## 默认快捷键

| 功能 | 快捷键 | 功能 | 快捷键 |
|---|---|---|---|
| 显示/隐藏窗口 | `Ctrl+Alt+F` | 新建标签 | `Ctrl+T` |
| 后退 / 前进 | `Alt+←` / `Alt+→` / `Alt+↓` | 上一级目录 | `Alt+↑` |
| 复制 / 剪切 / 粘贴 | `Ctrl+C` / `Ctrl+X` / `Ctrl+V` | 全选 | `Ctrl+A` |
| 重命名 | `F2` | 删除 | `Delete` |
| 永久删除 | `Shift+Delete` | 刷新 | `F5` |
| 新建文件夹 | `Ctrl+Shift+N` | 切换视图 | `Ctrl+Shift+V` |
| 定位地址栏 | `Ctrl+L` | 定位搜索框 | `Ctrl+F` |

全部 18 条均可在「设置 → 快捷键」中修改。

## 已知问题

- 插件的 `call` / `result` 能力调用链路尚未实现（插件可被发现和启动，但调不到能力）
- 取消传输操作会残留不完整的目标文件
