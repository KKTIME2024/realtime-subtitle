import os
import sys
from unittest.mock import MagicMock, patch

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from overlay_manager import DesktopOverlayManager


class TestDesktopOverlayManager:
    def test_build_command_uses_overlay_window_script(self):
        mgr = DesktopOverlayManager("http://127.0.0.1:8080")
        with patch.object(sys, "frozen", False, create=True):
            cmd = mgr._build_command(hidden=True)
        assert cmd[-1] == "--hidden"
        assert cmd[-4] == "--run-overlay"
        assert cmd[-3] == "--url"
        assert cmd[-2] == "http://127.0.0.1:8080"

    def test_open_then_close(self):
        mgr = DesktopOverlayManager("http://127.0.0.1:8080")
        proc = MagicMock()
        proc.poll.return_value = None
        with patch("subprocess.Popen", return_value=proc) as popen:
            assert mgr.open(hidden=True) is True
            popen.assert_called_once()
            assert mgr.is_open() is True
            assert mgr.close() is False
            proc.terminate.assert_called_once()
