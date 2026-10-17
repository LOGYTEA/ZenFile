---
title: ZenFile API
---

# ZenFile API

插件运行时协议。协议版本 `1`（`zenfile_core::PROTOCOL_VERSION`）。

::: warning 当前限制
**能力调用链路（`call` / `result`）尚未实现。** 宿主侧读线程目前只认 `hello` 和 `beat`，
收到 `call` 不会真的分发给插件。

当前可验收的范围是：**装得上、看得见、进程能拉起、能握手、能持续心跳、崩了能兜住**。

同理，`zenfile-core/src/ipc.rs` 里的长度前缀帧（4 字节 LE 长度 + JSON）与
`Envelope` 枚举**只在单元测试和自检里使用**，是给后续演进预留的正式帧格式，
**不是**当前生效的传输格式。
:::

## 命名管道

```
\\.\pipe\ZenFile.<pid>.<safe_id>
```

`<pid>` 是**插件自己的** pid。用 pid 是为了让「重启同一个插件」天然换一条管道，
不会撞上上一条的残留。

::: danger 这里有个非常容易踩的坑
两侧**必须调同一个函数** `zenfile_core::plugin::pipe_name(pid, id)` 计算这个名字。
早期宿主侧直接拼原始 id、插件侧过滤过字符，id 带非安全字符时两边算出两个名字，
插件永远连不上。

`safe_id` 的规则是：`[a-z0-9_-]` 保留，其余字符替换为 `_`。
:::

## 消息格式

传输是**换行（`\n`）分隔的 JSON**，一行一条消息。

| `t` | 方向 | 载荷 | 说明 |
|---|---|---|---|
| `hello` | 插件 → 宿主 | `{"t":"hello","id":...,"protocol":1,"argv":[...]}` | 握手，声明身份与协议版本 |
| `beat` | 插件 → 宿主 | `{"t":"beat"}` | 心跳 |
| `shutdown` | 宿主 → 插件 | — | 插件收到后应干净退出，返回 `0` |
| `call` | 宿主 → 插件 | `{"t":"call","id":N,...}` | 一次能力调用，**必须回响应** |
| `result` | 插件 → 宿主 | `{"t":"result","id":N,"ok":bool,"payload":...}` | 对 `call` 的响应 |

### 三条必须遵守的协议纪律

1. **心跳必须自己主动发，不能等宿主来问。** 宿主判断「插件还活着」的唯一依据就是这条。
   只在收到消息时才发心跳，宿主一安静就会误判卡死。间隔取超时的一半（`2500ms`），
   丢一拍不会被误杀。

2. **收到 `call` 必须回响应，哪怕回失败。** 不回的话宿主那边挂着一个等不到的请求。

3. **读消息时半截 JSON 要留着等下一次。** 管道是字节流，一次读拿到的很可能只是半条消息，
   直接解析会报 `EOF while parsing`——那不是协议错误，只是正常分片。

### `ConnectNamedPipe` 不能漏

::: danger
**`CreateNamedPipeW` 之后必须调 `ConnectNamedPipe`，否则数据永远不流动。**
:::

`CreateNamedPipeW` 只是造出一个「待连接」的实例；客户端 `CreateFileW` 挂上来的那一刻
连接还在排队状态，要 `ConnectNamedPipe` 才真正接通。

漏掉这一步的现象**极具欺骗性**：管道看起来存在、插件进程活着、没有任何错误日志，
但宿主一个字节都读不到——心跳永远停在初始值，5 秒后被判卡死。

客户端可能在 `ConnectNamedPipe` 之前就连上了，此时返回 `ERROR_PIPE_CONNECTED(535)`——
那是**成功**，不是错误，别当失败处理。

## 生命周期状态机

```
                     discover()  整个进程只做一次
                         │
                         ▼
   清单损坏 ──────────► Failed  (面板可见，不静默跳过)
                         │
   协议不符 ──────────► Failed  "协议版本不兼容"
                         │
                    拉起进程 ──► 拿到 pid ──► 建管道 ──► ConnectNamedPipe
                         │
                    Starting ──── 心跳超时 ──► Hung ──► kill ──► 重启
                         │
                  收到 hello（greeted）
                         ▼
                      Running
```

崩溃处理在 100ms 定时器里：

1. 先判**进程还在不在**，再判心跳（顺序不能反，见 [插件页](/guide/plugins#崩溃隔离)）
2. 进程没了 → 崩溃计数 +1 → 连续 4 次后停止重启（`MAX_RESTARTS = 3`）

## 能力

| 字符串 | 含义 |
|---|---|
| `clipboard.history` | 读剪贴板历史 |
| `screenshot.capture` | 触发截图 |
| `taskmgr.snapshot` | 拉取进程 / 性能快照 |
| `terminal.open` | 在当前目录开终端 |
| `preview.open` | 请求预览文件 |
| `core.log` | 写一行日志 |

能力名**点号小写与 PascalCase 两种写法都收**（`clipboard.history` 与
`ClipboardHistory` 等价）。写错时报错会列出全部可用名字：

```
未知能力 "no.such.cap"，可用：clipboard.history / screenshot.capture / ...
```

## 能力边界

插件进程里**没有窗口、没有托盘、没有单实例互斥**（`main.rs` 的 `--plugin` 分支在那些
逻辑之前就 `return`）。它拿不到宿主窗口句柄、托盘、别的插件的管道。

## 相关源码

| 位置 | 职责 |
|---|---|
| `zenfile-core/src/plugin.rs` | 清单解析、状态机、进程生命周期、管道名计算 |
| `zenfile-core/src/process.rs` | `ProcessTree`（Job Object）、`spawn`、`terminate_pid`、`process_alive` |
| `zenfile-app/src/plugin_server.rs` | **宿主侧**管道 server |
| `zenfile-app/src/plugin_host.rs` | **插件侧**：`PipeClient`、握手、心跳循环 |
| `zenfile-app/src/settings.rs` | 「插件」页绘制与开关 |
| `zenfile-core/src/ipc.rs` | 帧协议与 `Capability`（⚠️ 当前仅测试使用） |
