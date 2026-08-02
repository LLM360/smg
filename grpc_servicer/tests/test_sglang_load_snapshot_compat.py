"""Engine-free tests for SGLang load-snapshot field compatibility."""

from __future__ import annotations

import importlib.util
from pathlib import Path
from types import ModuleType, SimpleNamespace

_UTILS_PATH = Path(__file__).resolve().parent.parent / "smg_grpc_servicer" / "sglang" / "utils.py"


def _load_utils() -> ModuleType:
    spec = importlib.util.spec_from_file_location(
        "_smg_sglang_load_snapshot_utils_under_test", _UTILS_PATH
    )
    utils = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(utils)
    return utils


def test_prefill_prealloc_queue_uses_current_field() -> None:
    metrics = SimpleNamespace(
        prefill_prealloc_queue_reqs=7,
        prefill_bootstrap_queue_reqs=3,
    )
    assert _load_utils().prefill_prealloc_queue_reqs(metrics) == 7


def test_prefill_prealloc_queue_accepts_sglang_0_5_16_field() -> None:
    metrics = SimpleNamespace(prefill_bootstrap_queue_reqs=5)
    assert _load_utils().prefill_prealloc_queue_reqs(metrics) == 5


def test_prefill_prealloc_queue_defaults_to_zero() -> None:
    assert _load_utils().prefill_prealloc_queue_reqs(SimpleNamespace()) == 0
