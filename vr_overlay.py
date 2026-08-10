"""VR 浮层后端协议: 字幕事件 → snapshot → /vr_ws 广播。

VROverlay 是 ForeignSpeech 事件流的订阅者 (由 ipc_server.broadcast_foreign_speech
尾部挂接)。只负责「字幕该以什么参数显示」; 空间坐标/角度换算/SteamVR 状态
全部在 Rust 侧 (vr_overlay/), 见设计文档 §2 边界原则。
"""

from __future__ import annotations

import asyncio
from typing import Awaitable, Callable, Optional

CALIBRATION_DEFAULT = {
    "anchor": "head_locked",
    "offset_x": 0.0,
    "offset_y": -0.55,
    "distance": 1.1,
    "text_scale": 1.0,
    "background_alpha": 0.24,
}


def blank_block(seq: int) -> dict:
    return {
        "id": f"rt:clear:{seq}",
        "occupant_key": "peer:realtime",
        "appearance_seq": seq,
        "channel": "peer",
        "block_variant": "finalized",
        "primary_text": "",
        "secondary_text": "",
        "secondary_enabled": False,
    }


class VROverlay:
    """字幕状态机: 事件 → snapshot, 8s 无新字幕清屏。"""

    def __init__(
        self,
        broadcast: Callable[[dict], Awaitable[None]],
        clear_after: float = 8.0,
    ) -> None:
        self.broadcast = broadcast
        self.clear_after = clear_after
        self.revision = 0
        self._latest: dict = self._snapshot(0, [blank_block(0)])
        self._clear_task: Optional[asyncio.Task] = None

    def latest_snapshot(self) -> dict:
        """新连接 resync 用 (返回当前快照的浅拷贝)。"""
        return dict(self._latest)

    async def push(
        self,
        source_text: str,
        detected_language: Optional[str] = None,
        translation: Optional[str] = None,
    ) -> None:
        """FOREIGN_SPEECH 事件 → snapshot (主行=译文, 副行=原文)。"""
        if not source_text and not translation:
            return
        if self._clear_task is not None:
            self._clear_task.cancel()
            self._clear_task = None
        self.revision += 1
        translation_present = bool(translation)
        block = {
            "id": f"rt:{self.revision}",
            "occupant_key": "peer:realtime",
            "appearance_seq": self.revision,
            "channel": "peer",
            "block_variant": "finalized",
            "primary_text": translation or source_text,
            "secondary_text": source_text if translation_present else "",
            "secondary_enabled": translation_present,
            "primary_language": None,
            "secondary_language": detected_language if translation_present else None,
        }
        self._latest = self._snapshot(self.revision, [block])
        await self.broadcast(self._latest)
        if self.clear_after and self.clear_after > 0:
            self._clear_task = asyncio.create_task(self._clear_later())

    async def _clear_later(self) -> None:
        try:
            await asyncio.sleep(self.clear_after)
        except asyncio.CancelledError:
            return
        self.revision += 1
        self._latest = self._snapshot(self.revision, [blank_block(self.revision)])
        await self.broadcast(self._latest)

    def _snapshot(self, revision: int, blocks: list) -> dict:
        return {
            "type": "snapshot",
            "payload": {
                "revision": revision,
                "calibration": dict(CALIBRATION_DEFAULT),
                "blocks": blocks,
            },
        }
