from __future__ import annotations

import argparse
import re
import sys

from .bilibili_live import BilibiliLivePoller, iter_poll


def _parse_room_id(value: str) -> int:
    s = str(value or "").strip()
    if not s:
        return 0
    if s.isdigit():
        return int(s)
    m = re.search(r"live\.bilibili\.com/(\d+)", s)
    if m:
        return int(m.group(1))
    return 0


def _parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(prog="mori-live-stream", description="Minimal Bilibili live danmaku poller.")
    p.add_argument("--room-id", type=int, default=0, help="Bilibili live room id.")
    p.add_argument("--room-url", default="", help="Optional bilibili live URL to extract room id.")
    p.add_argument("--interval", type=float, default=2.0, help="Polling interval in seconds.")
    p.add_argument("--stop-after", type=float, default=0.0, help="Stop after seconds (0 = run forever).")
    p.add_argument("--include-history", action="store_true", help="Print current room messages once at startup.")
    args = p.parse_args()

    if int(args.room_id or 0) <= 0 and str(args.room_url or "").strip():
        args.room_id = _parse_room_id(str(args.room_url))

    if int(args.room_id or 0) <= 0:
        raise SystemExit("Missing --room-id (or pass --room-url like https://live.bilibili.com/<id>)")
    return args


def main() -> int:
    args = _parse_args()
    poller = BilibiliLivePoller(room_id=int(args.room_id))
    try:
        if bool(args.include_history):
            msgs = poller.fetch()
            for msg in sorted(msgs, key=lambda m: (float(m.ts or 0.0), str(m.timeline), str(m.nickname), str(m.text))):
                sys.stdout.write(f"[{msg.timeline}] {msg.nickname}: {msg.text}\n")
            sys.stdout.flush()
        for msg in iter_poll(poller, interval_s=float(args.interval), stop_after_s=float(args.stop_after)):
            sys.stdout.write(f"[{msg.timeline}] {msg.nickname}: {msg.text}\n")
            sys.stdout.flush()
    except KeyboardInterrupt:
        return 130
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
