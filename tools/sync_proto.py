#!/usr/bin/env python3
"""把 aoxchat 的协议 crate 同步到 AOX 仓库里的副本。

    python tools/sync_proto.py --from /root/aoxchat/proto

为什么要副本：AOX 与 aoxchat 是两个仓库，AOX 必须能离线构建；让 AOX 直接 path 依赖
另一个仓库的路径，换台机器就编译不过。所以这里做**单向复制**，并写入内容哈希，
两边不一致时能立刻看出来。
"""

from __future__ import annotations

import argparse
import hashlib
import shutil
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEST = ROOT / "crates" / "aoxchat-proto"


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--from", dest="src", required=True, help="aoxchat 仓库根目录")
    ap.add_argument("--check", action="store_true", help="只比较，不写入")
    args = ap.parse_args()

    src = Path(args.src).expanduser() / "proto" / "src" / "lib.rs"
    if not src.is_file():
        print(f"找不到 {src}", file=sys.stderr)
        return 2

    dst = DEST / "src" / "lib.rs"
    a = hashlib.sha256(src.read_bytes()).hexdigest()
    b = hashlib.sha256(dst.read_bytes()).hexdigest() if dst.is_file() else ""

    if a == b:
        print("already in sync")
        return 0
    if args.check:
        print(f"OUT OF SYNC\n  {src}\n  {dst}", file=sys.stderr)
        return 1

    dst.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(src, dst)
    print(f"synced {src} -> {dst}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
