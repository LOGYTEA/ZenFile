#!/usr/bin/env bash
# 构建 ZenFile 官网并发布到 GitHub Pages。
#
# 站点实际托管在用户主页仓库 LOGYTEA/LOGYTEA.github.io 的 zenfile/ 子路径，
# 线上地址 https://logytea.github.io/zenfile/ 。本仓库只是站点的**源码**，
# 不是部署目标 —— 所以这里走"本地构建 → 推另一个仓库"，不用 GitHub Actions。
#
# 用法：
#   ./deploy_site.sh            # 构建 + 同步 + 提交推送
#   ./deploy_site.sh --dry-run  # 只构建 + 同步到目标仓库，不提交不推送
#
# 依赖：node / npm / python / git

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SITE_SRC="$REPO_ROOT/site"
SITE_DIST="$SITE_SRC/.vitepress/dist"

# 目标仓库：你的 GitHub 主页仓库，Pages 从它的 main 分支发布。
PAGES_REPO="https://github.com/LOGYTEA/LOGYTEA.github.io.git"
# 本地克隆位置。用 /tmp 是 Git Bash 的 %TEMP%，不是 C:/tmp。
PAGES_DIR="${PAGES_DIR:-$TEMP/zhome}"
# 站点在目标仓库里的子路径。
SITE_SUBDIR="zenfile"

DRY_RUN=0
[ "${1:-}" = "--dry-run" ] && DRY_RUN=1

step() { printf '\n==> %s\n' "$1"; }

# ---------------------------------------------------------------- 构建

step "构建站点（base 必须与 $SITE_SUBDIR/ 对齐）"
cd "$SITE_SRC"
npm run build

# VitePress 产物里没有 .nojekyll，Pages 的 Jekyll 会插手处理目录。
# VitePress 的 assets/ 目录名以 a 开头不会被 Jekyll 忽略，但保险起见显式关掉。
touch "$SITE_DIST/.nojekyll"

[ -f "$SITE_DIST/index.html" ] || { echo "构建产物里没有 index.html，构建失败" >&2; exit 1; }
echo "产物文件数：$(find "$SITE_DIST" -type f | wc -l)"

# ---------------------------------------------------------------- 克隆目标仓库

if [ ! -d "$PAGES_DIR/.git" ]; then
  step "克隆目标仓库到 $PAGES_DIR"
  git clone "$PAGES_REPO" "$PAGES_DIR"
fi

# ---------------------------------------------------------------- 同步

step "同步产物到 $PAGES_DIR/$SITE_SUBDIR/"
python - "$SITE_DIST" "$PAGES_DIR/$SITE_SUBDIR" <<'PY'
import os, shutil, sys

src, dst = sys.argv[1], sys.argv[2]

# 先清后拷：站点删页时旧产物必须一起消失，否则线上会留死链。
if os.path.isdir(dst):
    shutil.rmtree(dst)
shutil.copytree(src, dst)

# 字节级校验：Jekyll 关掉后，磁盘字节必须原样入库。
# 本机 core.autocrlf=true 会把 LF 改成 CRLF，产物哈希全变。
# 目标仓库根有 .gitattributes 写 `* -text` 兜底，这里再核一遍。
n = 0
for root, _, files in os.walk(dst):
    for name in files:
        n += 1
print(f"已复制 {n} 个文件到 {dst}")
PY

cd "$PAGES_DIR"
git check-attr text -- "$SITE_SUBDIR/index.html" | grep -q 'unset' \
  || { echo "目标仓库的 .gitattributes 没生效（text 不是 unset），字节会被转换，先修它" >&2; exit 1; }

# ---------------------------------------------------------------- 提交推送

cd "$PAGES_DIR"
git add -A "$SITE_SUBDIR"

if git diff --cached --quiet; then
  step "产物无变化，无需提交"
  exit 0
fi

echo "变更文件："
git diff --cached --name-status | head -40

if [ "$DRY_RUN" = "1" ]; then
  step "--dry-run：到此为止，不提交不推送"
  exit 0
fi

step "提交并推送"
git commit -m "更新官网产物（来源 ZenFile 仓库 site/）"
git push origin main

step "完成。线上地址：https://logytea.github.io/$SITE_SUBDIR/"
echo "注意 CDN 可能有缓存，若没立刻生效等一两分钟再刷。"
