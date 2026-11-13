"""smg.tokenizer.Tokenizer K3.1 serving-reference parity.

The fixture `tests/fixtures/kimi_k3/k31_serving_reference.json` in the
`llm-tokenizer` crate records, for every case group the gateway must serve, the
bytes and token IDs that the pinned Kimi-K3.1-Pro checkpoint
(`moonshotai/Kimi-K3.1-Pro` at `6d0a0202956796543364d666ab19c6ad1a494ee1`)
renders through its own `encoding_k3.py`. This module renders the same requests
through the public binding and compares text and token IDs without trimming,
normalization or JSON equality.

The cases need the checkpoint's tokenizer files, which the suite does not
write. `KIMI_K31_MODEL_DIR` points at a local checkpoint directory. Without it
the whole module skips, so the rest of the suite keeps running offline. The
image probe and CI set the variable from the fixture cache.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Any

import pytest

pytest.importorskip("smg.smg_rs")

from smg.tokenizer import Tokenizer

REPOSITORY = "moonshotai/Kimi-K3.1-Pro"
REVISION = "6d0a0202956796543364d666ab19c6ad1a494ee1"

FIXTURE = (
    Path(__file__).resolve().parents[3]
    / "crates/tokenizer/tests/fixtures/kimi_k3/k31_serving_reference.json"
)


def _snapshot() -> Path | None:
    value = os.environ.get("KIMI_K31_MODEL_DIR")
    if not value:
        return None
    path = Path(value)
    if not (path / "tiktoken.model").is_file():
        return None
    return path


@pytest.fixture(scope="module")
def reference() -> dict[str, Any]:
    return json.loads(FIXTURE.read_text(encoding="utf-8"))


@pytest.fixture(scope="module")
def k31() -> Tokenizer:
    snapshot = _snapshot()
    if snapshot is None:
        pytest.skip("KIMI_K31_MODEL_DIR is not set: no K3.1 checkpoint files")
    return Tokenizer(snapshot, model=REPOSITORY)


def _binding_cases(reference: dict[str, Any]) -> list[tuple[str, dict[str, Any]]]:
    cases = reference["cases"]
    return [
        (name, case)
        for name, case in sorted(cases.items())
        if case["transport"] == "binding"
    ]


def test_fixture_names_the_pinned_checkpoint(reference: dict[str, Any]) -> None:
    assert reference["repository"] == REPOSITORY
    assert reference["revision"] == REVISION


@pytest.mark.parametrize(
    ("name", "case"),
    _binding_cases(json.loads(FIXTURE.read_text(encoding="utf-8"))),
    ids=lambda value: value if isinstance(value, str) else None,
)
def test_a_binding_case_matches_the_reference(
    k31: Tokenizer, name: str, case: dict[str, Any]
) -> None:
    rendered = k31.render_chat(case["request"])
    assert rendered["text"] == case["expected_text"], (
        f"{name}: rendered bytes differ from the reference"
    )
    assert rendered["token_ids"] == case["expected_token_ids"], (
        f"{name}: token IDs differ from the reference"
    )


def test_the_image_case_is_media_and_the_binding_refuses_it(
    reference: dict[str, Any],
) -> None:
    # The reference records the image case through the lower-level path. The
    # public binding refuses media content on purpose, so the byte-for-byte
    # proof for that case lives in the Rust test. Here the binding must still
    # refuse it with the gateway's reason rather than silently drop the part.
    case = reference["cases"]["image_part"]
    assert case["transport"] == "media"
    snapshot = _snapshot()
    if snapshot is None:
        pytest.skip("KIMI_K31_MODEL_DIR is not set: no K3.1 checkpoint files")
    tokenizer = Tokenizer(snapshot, model=REPOSITORY)
    with pytest.raises(ValueError):
        tokenizer.render_chat(case["request"])