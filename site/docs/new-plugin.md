# 开始你的第一个插件

这是**照着能做完**的操作路径，大约 5 分钟。

## 第 1 步：建目录

在 `ZenFile.exe` **同级**建 `plugins/<你的插件 id>/`，里面放 `plugin.json`。

::: tip 用 Git Bash
```bash
EXE_DIR="/c/Users/Administrator/Desktop/project/File/target/release"
ID="hello-demo"
mkdir -p "$EXE_DIR/plugins/$ID"
```
:::

::: warning
目录必须在 exe **旁边**，不是 exe 的工作目录。放错位置面板会显示「没有找到插件」。
:::

## 第 2 步：写 plugin.json

在 `$EXE_DIR/plugins/hello-demo/plugin.json` 写入（**UTF-8、无 BOM、不加注释**）：

```json
{
  "id": "hello-demo",
  "name": "你好插件",
  "description": "第一个插件，用来验证心跳链路",
  "protocol_version": 1,
  "autostart": true,
  "capabilities": ["core.log"]
}
```

## 第 3 步：这就是全部

**插件不需要编译任何代码。** 因为插件是 `ZenFile.exe` 的另一个进程，
你写的这份 `plugin.json` 就是插件的全部。

宿主启动时会自动执行：

```bash
ZenFile.exe --plugin hello-demo
```

## 第 4 步：验证成功

打开 ZenFile →「设置 - 插件」，应看到：

```
你好插件
运行中                    ← 关键：不是「启动中」，更不是「启动失败」
```

**唯一可靠的判据**是看日志，面板文字可能骗人：

```bash
grep "\[beat\]" "$TEMP/zenfile_plugins.log" | tail -5
```

期望输出形如：

```
[beat] id=hello-demo now=139485015 last_beat=139484921 age_ms=94 greeted=true pipe=\\.\pipe\ZenFile.19304.hello-demo
```

逐项判读：

| 字段 | 期望值 | 不对说明什么 |
|---|---|---|
| `greeted` | `true` | `false` = 握手没完成，插件进程没连上 |
| `age_ms` | 远小于 `5000` | 持续涨到 5000 = 心跳断了，会被判「无响应」 |
| `pipe` | `\\.\pipe\ZenFile.<插件 pid>.<id>` | 管道名含插件自己的 pid |

### 退出码：定位失败在哪一步

插件进程自己退出时，退出码直接告诉你是哪一步挂了：

```bash
"$EXE_DIR/ZenFile.exe" --plugin hello-demo; echo "EXIT=$?"
```

| 码 | 含义 | 怎么办 |
|---|---|---|
| `0` | 正常收到 `shutdown` | 宿主主动停的，正常 |
| `2` | `--plugin` 后面没有合法 id | 检查命令行 |
| `3` | 管道连不上 | 宿主那边没建管道，或宿主已经退了 |

::: warning
手动跑时**几乎总是 `3`**——管道名含插件自己的 pid，手动跑的这个进程没有宿主为它建管道。
退出码 3 要在**自动路径**下才有意义，别拿手动跑的结果下结论。
:::

## 下一步

- 协议细节、能力清单、状态机 → [ZenFile API](/docs/api)
- 宿主架构与传输引擎 → [架构设计](/docs/architecture)
