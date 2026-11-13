#!/usr/bin/env python3
"""Regenerate the K3.1 serving reference vectors.

The fixtures record what the pinned Kimi-K3.1-Pro checkpoint's own encoder
(`encoding_k3.py` plus `tokenization_kimi.py`) renders for each case. The Rust
renderer and the Python binding compare their output with these bytes and token
IDs, so the committed file is the only authority a test needs. No test reads the
network or the checkpoint.

Regeneration needs an explicit local snapshot of the checkpoint at the pinned
revision. Pass the snapshot directory that holds `encoding_k3.py`,
`tokenization_kimi.py`, `tiktoken.model`, `tokenizer_config.json` and
`config.json`:

    python crates/tokenizer/tests/fixtures/kimi_k3/generate_k31_reference.py \
        --snapshot /path/to/Kimi-K3.1-Pro
    python crates/tokenizer/tests/fixtures/kimi_k3/generate_k31_reference.py \
        --snapshot /path/to/Kimi-K3.1-Pro --check

`--check` renders the cases and compares the result with the committed file. It
writes nothing. A snapshot of any other revision is refused: the fixture records
the revision and the test names it.

The developer-message case follows the gateway: SMG rewrites a `developer`
message as a `system` one and merges a non-leading system message into the
first. The reference encoder has no `developer` branch, so the generator applies
the same rewrite the gateway applies before it calls the encoder.
"""

from __future__ import annotations

import argparse
import json
import sys
from collections.abc import Iterable
from pathlib import Path

REPOSITORY = "moonshotai/Kimi-K3.1-Pro"
REVISION = "6d0a0202956796543364d666ab19c6ad1a494ee1"

FIXTURE = Path(__file__).with_name("k31_serving_reference.json")

# One entry per case: the name, the chat body the gateway receives, the encoder
# options, and the transport that renders it. "binding" cases go through
# `smg.tokenizer.Tokenizer.render_chat`; "media" cases go through the
# lower-level `TiktokenTokenizer` path, because the binding refuses media
# content on purpose.
TOOLS = [
    {
        "type": "function",
        "function": {
            "name": "get_weather",
            "description": "Report the weather for one city.",
            "parameters": {
                "type": "object",
                "properties": {
                    "city": {"type": "string"},
                    "units": {"type": "string", "enum": ["c", "f"]},
                },
                "required": ["city"],
            },
        },
    }
]


def cases() -> list[dict]:
    """Every case group the fixture covers, in a stable order."""
    return [
        # 1. Plain user request: the served default is thinking_effort=max.
        {
            "name": "plain_default_effort_max",
            "group": "plain",
            "transport": "binding",
            "body": {"messages": [{"role": "user", "content": "Hi"}]},
            "encoder": {"thinking": True, "thinking_effort": "max"},
        },
        {
            "name": "plain_no_thinking",
            "group": "plain",
            "transport": "binding",
            "body": {"messages": [{"role": "user", "content": "Hi"}], "thinking": False},
            "encoder": {"thinking": False},
        },
        # 2. System-role preservation.
        {
            "name": "system_preserved",
            "group": "system",
            "transport": "binding",
            "body": {
                "messages": [
                    {"role": "system", "content": "You are a careful assistant."},
                    {"role": "user", "content": "Name the colour of the sky."},
                ]
            },
            "encoder": {"thinking": True, "thinking_effort": "max"},
        },
        # 3. Multiple turns with prior assistant reasoning content.
        {
            "name": "multi_turn_reasoning_content",
            "group": "multi_turn",
            "transport": "binding",
            "body": {
                "messages": [
                    {"role": "user", "content": "What is 17 times 4?"},
                    {
                        "role": "assistant",
                        "reasoning_content": "17 times 4 is 17 times 2 doubled.",
                        "content": "68",
                    },
                    {"role": "user", "content": "And minus 8?"},
                ]
            },
            "encoder": {"thinking": True, "thinking_effort": "max"},
        },
        # 4. Tool definitions, calls and results.
        {
            "name": "tools_declared",
            "group": "tools",
            "transport": "binding",
            "body": {
                "tools": TOOLS,
                "messages": [{"role": "user", "content": "Weather in Lisbon?"}],
            },
            "encoder": {"thinking": True, "thinking_effort": "max"},
        },
        {
            "name": "tools_call_and_result",
            "group": "tools",
            "transport": "binding",
            "body": {
                "tools": TOOLS,
                "messages": [
                    {"role": "user", "content": "Weather in Lisbon, in celsius?"},
                    {
                        "role": "assistant",
                        "tool_calls": [
                            {
                                "id": "call_1",
                                "type": "function",
                                "function": {
                                    "name": "get_weather",
                                    "arguments": '{"city": "Lisbon", "units": "c"}',
                                },
                            }
                        ],
                    },
                    {
                        "role": "tool",
                        "tool_call_id": "call_1",
                        "content": '{"celsius": 21}',
                    },
                ],
            },
            "encoder": {"thinking": True, "thinking_effort": "max"},
        },
        # 5. Omitted effort and every accepted value. An omitted effort
        # request leaves the gateway's served default, which the checkpoint
        # tokenizer's `apply_chat_template` sets to `max`.
        {
            "name": "effort_omitted_defaults_to_max",
            "group": "effort",
            "transport": "binding",
            "body": {"messages": [{"role": "user", "content": "Hi"}]},
            "encoder": {"thinking": True, "thinking_effort": "max"},
        },
        {
            "name": "effort_low",
            "group": "effort",
            "transport": "binding",
            "body": {
                "messages": [{"role": "user", "content": "Hi"}],
                "reasoning_effort": "low",
            },
            "encoder": {"thinking": True, "thinking_effort": "low"},
        },
        {
            "name": "effort_medium",
            "group": "effort",
            "transport": "binding",
            "body": {
                "messages": [{"role": "user", "content": "Hi"}],
                "reasoning_effort": "medium",
            },
            "encoder": {"thinking": True, "thinking_effort": "medium"},
        },
        {
            "name": "effort_high",
            "group": "effort",
            "transport": "binding",
            "body": {
                "messages": [{"role": "user", "content": "Hi"}],
                "reasoning_effort": "high",
            },
            "encoder": {"thinking": True, "thinking_effort": "high"},
        },
        {
            "name": "effort_max",
            "group": "effort",
            "transport": "binding",
            "body": {
                "messages": [{"role": "user", "content": "Hi"}],
                "reasoning_effort": "max",
            },
            "encoder": {"thinking": True, "thinking_effort": "max"},
        },
        # 5b. The reference's own refusal of an unsupported effort. The
        # checkpoint raises before it writes a segment, so the fixture records
        # the refusal text and no bytes.
        {
            "name": "effort_invalid_is_refused",
            "group": "effort",
            "transport": "binding",
            "body": {
                "messages": [{"role": "user", "content": "Hi"}],
                "reasoning_effort": "minimal",
            },
            "encoder": {"thinking": True, "thinking_effort": "minimal"},
            "expect_refusal": True,
        },
        # 6. Image content through the lower-level tokenizer path.
        {
            "name": "image_part",
            "group": "image",
            "transport": "media",
            "body": {
                "messages": [
                    {
                        "role": "user",
                        "content": [
                            {"type": "text", "text": "Describe the image in one sentence."},
                            {"type": "image"},
                        ],
                    }
                ],
                "thinking": False,
            },
            "encoder": {"thinking": False},
            "image_prompts": ["<|media_pad|>"],
        },
        # 7. Developer-role normalization.
        {
            "name": "developer_as_system",
            "group": "developer",
            "transport": "binding",
            "body": {
                "messages": [
                    {"role": "developer", "content": "Answer in one short sentence."},
                    {"role": "user", "content": "Colour of the sky?"},
                ]
            },
            "encoder": {"thinking": True, "thinking_effort": "max"},
            "gateway_rewrite": "developer_as_system",
        },
        # 8. Structured tool arguments: numeric spelling and nesting.
        {
            "name": "tool_arguments_structured",
            "group": "tool_arguments",
            "transport": "binding",
            "body": {
                "tools": TOOLS,
                "messages": [
                    {"role": "user", "content": "Record the reading."},
                    {
                        "role": "assistant",
                        "tool_calls": [
                            {
                                "id": "call_2",
                                "type": "function",
                                "function": {
                                    "name": "get_weather",
                                    "arguments": (
                                        '{"city":"Oslo","nested":{"a":[1,2,{"b":null}]},'
                                        '"flag":true,"count":7,"ratio":1.0,'
                                        '"neg_zero":-0.0,"exp":1e2,"neg":-3}'
                                    ),
                                },
                            }
                        ],
                    },
                    {"role": "tool", "tool_call_id": "call_2", "content": "ok"},
                ],
            },
            "encoder": {"thinking": True, "thinking_effort": "max"},
        },
    ]


def developer_as_system(messages: list[dict]) -> list[dict]:
    """The gateway's developer rewrite: role becomes `system`, `tools` drops, and
    a system message that is not first moves to the front and merges."""
    if not any(m.get("role") == "developer" for m in messages):
        return messages
    rewritten: list[dict] = []
    for message in messages:
        if message.get("role") != "developer":
            rewritten.append(dict(message))
            continue
        out = dict(message)
        out["role"] = "system"
        out.pop("tools", None)
        rewritten.append(out)
    leading = [m for m in rewritten if m.get("role") != "system"]
    systems = [m for m in rewritten if m.get("role") == "system"]
    if not systems:
        return rewritten
    if rewritten and rewritten[0].get("role") == "system" and len(systems) == 1:
        return rewritten
    merged_parts = []
    for message in systems:
        content = message.get("content")
        if isinstance(content, str):
            merged_parts.append(content)
        elif isinstance(content, list):
            merged_parts.extend(
                part.get("text", "") for part in content if isinstance(part, dict)
            )
    merged = {
        "role": "system",
        "content": "\n\n".join(part for part in merged_parts if part),
    }
    return [merged, *leading]


def check_revision(snapshot: Path) -> None:
    """Refuse a snapshot that is not the pinned revision.

    A local snapshot directory normally carries no git metadata, so the
    authority is the recorded revision beside the encoder when one exists and,
    always, the checkpoint's own `config.json` `model_type`. `--expect-revision`
    accepts a revision string that the snapshot's own manifest records.
    """
    revision_file = snapshot / "REVISION"
    if revision_file.exists():
        recorded = revision_file.read_text(encoding="utf-8").strip()
        if recorded != REVISION:
            raise SystemExit(
                f"snapshot records revision {recorded!r}, not {REVISION!r}"
            )


def load_reference(snapshot: Path):
    """Import the checkpoint's own encoder modules from the snapshot."""
    sys.path.insert(0, str(snapshot))
    try:
        import tiktoken  # noqa: F401
        from tokenizers import AddedToken
        from encoding_k3 import build_chat_segments
        from tokenization_kimi import TikTokenTokenizer
    finally:
        sys.path.pop(0)

    check_revision(snapshot)
    config = json.loads((snapshot / "config.json").read_text(encoding="utf-8"))
    if config.get("model_type") != "kimi_k31":
        raise SystemExit(
            f"snapshot model_type is {config.get('model_type')!r}, not 'kimi_k31'"
        )
    expected_arch = "KimiK31ForConditionalGeneration"
    if expected_arch not in (config.get("architectures") or []):
        raise SystemExit(
            f"snapshot architectures {config.get('architectures')!r} do not name {expected_arch}"
        )
    tokenizer_config = json.loads((snapshot / "tokenizer_config.json").read_text())
    decoder = {
        int(index): AddedToken(
            spec["content"],
            lstrip=spec["lstrip"],
            rstrip=spec["rstrip"],
            single_word=spec["single_word"],
            normalized=spec["normalized"],
            special=spec["special"],
        )
        for index, spec in tokenizer_config["added_tokens_decoder"].items()
    }
    tokenizer = TikTokenTokenizer(
        vocab_file=str(snapshot / "tiktoken.model"),
        bos_token="[BOS]",
        eos_token="[EOS]",
        unk_token="[UNK]",
        pad_token="[PAD]",
        added_tokens_decoder=decoder,
    )
    return build_chat_segments, tokenizer


def tool_names(messages: Iterable[dict]) -> dict[str, str]:
    """Map each assistant tool_call id to the function name the encoder reads."""
    names: dict[str, str] = {}
    for message in messages:
        for call in message.get("tool_calls") or []:
            call_id = call.get("id")
            function = call.get("function") or {}
            if call_id is not None and function.get("name"):
                names[str(call_id)] = function["name"]
    return names


def encoder_messages(case: dict, build_chat_segments, tokenizer) -> list[dict]:
    """The messages the encoder receives.

    A `tool` message that carries only `tool_call_id` is resolved to the tool
    name from the matching assistant call, as `tokenization_kimi.py` does. The
    role is left as the gateway leaves it (already rewritten when
    `gateway_rewrite` is set).
    """
    messages = [dict(m) for m in case["body"]["messages"]]
    names = tool_names(messages)
    for message in messages:
        if message.get("role") != "tool":
            continue
        if message.get("tool") or message.get("name"):
            continue
        call_id = message.get("tool_call_id")
        if call_id is not None and str(call_id) in names:
            message["tool"] = names[str(call_id)]
    if case.get("gateway_rewrite") == "developer_as_system":
        messages = developer_as_system(messages)
    elif any(m.get("role") == "developer" for m in messages):
        messages = developer_as_system(messages)
    return messages


def render_case(case: dict, build_chat_segments, tokenizer) -> dict:
    messages = encoder_messages(case, build_chat_segments, tokenizer)
    options = dict(case["encoder"])
    tools = case["body"].get("tools")
    image_prompts = case.get("image_prompts")
    if case.get("expect_refusal"):
        # The reference raises before it writes a segment. Record the error as
        # the case's expectation instead of bytes.
        try:
            build_chat_segments(
                [dict(m) for m in messages],
                tools=[dict(t) for t in tools] if tools else None,
                add_generation_prompt=case["body"].get("add_generation_prompt", True),
                image_prompts=image_prompts,
                **options,
            )
        except Exception as error:  # noqa: BLE001
            return {"refusal": type(error).__name__, "message": str(error)}
        raise SystemExit(f"case {case['name']}: the reference accepted what it must refuse")
    segments = build_chat_segments(
        [dict(m) for m in messages],
        tools=[dict(t) for t in tools] if tools else None,
        add_generation_prompt=case["body"].get("add_generation_prompt", True),
        image_prompts=image_prompts,
        **options,
    )
    text = "".join(segment.text for segment in segments)
    token_ids = tokenizer._encode_chat_segments(segments)
    return {"text": text, "token_ids": token_ids}


def build_fixture(snapshot: Path) -> dict:
    build_chat_segments, tokenizer = load_reference(snapshot)
    entries = {}
    for case in cases():
        rendered = render_case(case, build_chat_segments, tokenizer)
        entries[case["name"]] = {
            "group": case["group"],
            "transport": case["transport"],
            "request": case["body"],
            "encoder_options": case["encoder"],
            **({"image_prompts": case["image_prompts"]} if case.get("image_prompts") else {}),
            **({"gateway_rewrite": case["gateway_rewrite"]} if case.get("gateway_rewrite") else {}),
            **({"refusal": rendered["refusal"], "refusal_message": rendered["message"]}
               if "refusal" in rendered else {"expected_text": rendered["text"],
                                              "expected_token_ids": rendered["token_ids"]}),
        }
    return {
        "repository": REPOSITORY,
        "revision": REVISION,
        "generator": "encoding_k3.build_chat_segments + tokenization_kimi.TikTokenTokenizer._encode_chat_segments",
        "cases": entries,
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--snapshot", required=True, type=Path)
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--output", type=Path, default=FIXTURE)
    args = parser.parse_args(argv)

    fixture = build_fixture(args.snapshot)
    rendered = json.dumps(fixture, indent=2, ensure_ascii=False) + "\n"
    if args.check:
        committed = args.output.read_text(encoding="utf-8")
        if committed != rendered:
            print(f"fixture differs from {args.output}: rerun without --check", file=sys.stderr)
            return 1
        print(f"fixture matches {args.output}")
        return 0
    args.output.write_text(rendered, encoding="utf-8")
    print(f"wrote {args.output} ({len(fixture['cases'])} cases)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())