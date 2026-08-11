import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import config


def test_vr_overlay_enabled_env_parsing():
    old = os.environ.get("VR_OVERLAY_ENABLED")
    try:
        os.environ["VR_OVERLAY_ENABLED"] = "1"
        # 重新加载 config 以观察 env 生效
        import importlib
        importlib.reload(config)
        assert config.VR_OVERLAY_ENABLED is True

        os.environ["VR_OVERLAY_ENABLED"] = ""
        importlib.reload(config)
        assert config.VR_OVERLAY_ENABLED is False
    finally:
        if old is None:
            os.environ.pop("VR_OVERLAY_ENABLED", None)
        else:
            os.environ["VR_OVERLAY_ENABLED"] = old
        importlib.reload(config)
