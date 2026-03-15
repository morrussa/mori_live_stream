from __future__ import annotations

import json
import urllib.error
import urllib.request
from dataclasses import dataclass
from typing import Any


class BilibiliRoomError(RuntimeError):
    pass


@dataclass(frozen=True)
class RoomInfo:
    room_id: int
    live_status: int
    title: str
    online: int
    live_time: str
    raw: dict[str, Any]


def get_room_info(*, room_id: int, user_agent: str | None = None, timeout_s: int = 10) -> RoomInfo:
    ua = (user_agent or "").strip() or (
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) "
        "AppleWebKit/537.36 (KHTML, like Gecko) "
        "Chrome/120.0.0.0 Safari/537.36"
    )
    url = f"https://api.live.bilibili.com/room/v1/Room/get_info?room_id={int(room_id)}"
    req = urllib.request.Request(url, headers={"User-Agent": ua}, method="GET")

    try:
        with urllib.request.urlopen(req, timeout=timeout_s) as resp:
            payload = resp.read().decode("utf-8", errors="replace")
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", errors="replace") if getattr(e, "fp", None) else ""
        raise BilibiliRoomError(f"HTTP {e.code} from bilibili room/get_info: {body[:200]}") from e
    except Exception as e:
        raise BilibiliRoomError(f"Failed to fetch bilibili room/get_info: {e}") from e

    try:
        obj = json.loads(payload)
    except json.JSONDecodeError as e:
        raise BilibiliRoomError(f"Invalid JSON from bilibili room/get_info: {payload[:200]}") from e

    code = obj.get("code")
    if code != 0:
        raise BilibiliRoomError(f"Unexpected bilibili room/get_info code={code}: {payload[:200]}")

    data = obj.get("data")
    if not isinstance(data, dict):
        raise BilibiliRoomError("Invalid bilibili room/get_info payload (missing data).")

    return RoomInfo(
        room_id=int(data.get("room_id") or room_id),
        live_status=int(data.get("live_status") or 0),
        title=str(data.get("title") or ""),
        online=int(data.get("online") or 0),
        live_time=str(data.get("live_time") or ""),
        raw=data,
    )


def is_live(*, room_id: int, user_agent: str | None = None, timeout_s: int = 10) -> bool:
    return get_room_info(room_id=room_id, user_agent=user_agent, timeout_s=timeout_s).live_status == 1

