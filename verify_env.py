"""验证脚本共用：**隔离被测应用的配置**，绝不污染用户真实设置。

## 为什么会需要这个模块

脚本为了构造场景（比如"隐藏的项目=开"），会直接改写：

    %APPDATA%\\ZenFile\\settings.json        <- 应用层（图形设置页写的就是它）
    %APPDATA%\\ZenFile\\settings.user.json   <- 覆盖层（优先级最高）

跑完不还原，用户下次打开软件就带着脚本遗留的状态跑。实测后果：verify_round10
把 `default_mode` 留在了 `list`，于是之后所有文件夹都采用列表视图 —— 列表视图
本来就不画表头，看起来就是"打开某个文件夹时 名称/类型/大小 那一行没了"。
用户以为产品有缺陷，其实是验证脚本在他自己的配置目录里拉了屎。

## 用法

    import verify_env
    verify_env.isolate()     # 越早越好：备份现有配置，进程退出时原样还原

脚本中途崩溃也算 —— 注册的是 `atexit`，正常退出和异常退出都会走到。
"""
import atexit
import os

CFG_DIR = os.path.join(os.environ.get("APPDATA", os.path.expanduser("~")), "ZenFile")
FILES = ("settings.json", "settings.user.json")

_saved = {}
_registered = False


def isolate():
    """备份两个配置文件，并注册退出还原。"""
    global _registered
    for name in FILES:
        path = os.path.join(CFG_DIR, name)
        try:
            with open(path, "rb") as f:
                _saved[name] = f.read()
        except OSError:
            _saved[name] = None
    if not _registered:
        atexit.register(restore)
        _registered = True


def restore():
    """把配置文件还原到 isolate() 那一刻的状态（不存在就删掉）。"""
    for name, data in _saved.items():
        path = os.path.join(CFG_DIR, name)
        try:
            if data is None:
                if os.path.exists(path):
                    os.remove(path)
            else:
                os.makedirs(CFG_DIR, exist_ok=True)
                with open(path, "wb") as f:
                    f.write(data)
        except OSError:
            pass


def clean():
    """兜底清理：确保两个配置文件都不存在（用于脚本自己的临时场景）。"""
    for name in FILES:
        path = os.path.join(CFG_DIR, name)
        try:
            if os.path.exists(path):
                os.remove(path)
        except OSError:
            pass
