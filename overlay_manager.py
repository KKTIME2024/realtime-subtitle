"""原生桌面字幕悬浮窗子进程管理者（PySide6 overlay）。

与 pywebview 主窗口分属不同进程，避免两个 GUI 事件循环冲突。子进程通过
WebSocket(`/ws`) 接收字幕，并用 REST(`/pause`,`/resume`) 控制识别。
"""

import os
import sys
import subprocess


class DesktopOverlayManager:
    """管理原生（PySide6）字幕悬浮窗子进程。

    与 pywebview 主窗口分属不同进程，避免两个 GUI 事件循环冲突。子进程通过
    WebSocket(`/ws`) 接收字幕，并用 REST(`/pause`,`/resume`) 控制识别。
    """

    def __init__(self, server_url: str, web_server=None):
        self.server_url = server_url
        self.web_server = web_server
        self._proc = None
        self.is_visible = False

    def is_open(self) -> bool:
        return self._proc is not None and self._proc.poll() is None

    def _build_command(self, hidden: bool = True) -> list[str]:
        args = ["--run-overlay", "--url", self.server_url]
        if hidden:
            args.append("--hidden")
        if getattr(sys, "frozen", False):
            # PyInstaller：重新拉起自身可执行文件，由 main() 顶部分发到 overlay。
            return [sys.executable, *args]
        overlay_script = os.path.join(os.path.dirname(os.path.abspath(__file__)), "overlay_window.py")
        return [sys.executable, overlay_script, *args]

    def open(self, hidden: bool = False) -> bool:
        if self.is_open():
            return True
        import subprocess

        kwargs = {}
        if os.name == "nt":
            # 不为悬浮窗弹出额外的控制台窗口。
            kwargs["creationflags"] = 0x08000000  # CREATE_NO_WINDOW
        try:
            self._proc = subprocess.Popen(self._build_command(hidden=hidden), **kwargs)
        except Exception as error:
            print(f"⚠️  Failed to launch subtitle overlay: {error}")
            self._proc = None
            raise
        return True

    def prewarm(self):
        """Pre-warm the overlay process by starting it hidden."""
        try:
            self.open(hidden=True)
        except Exception as e:
            print(f"⚠️  Failed to pre-warm overlay: {e}")

    def close(self) -> bool:
        proc = self._proc
        self._proc = None
        if proc is not None and proc.poll() is None:
            try:
                proc.terminate()
            except Exception:
                pass
        return False
