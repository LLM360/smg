//! K3.1 serving-reference parity: the renderer's bytes and token IDs against the
//! pinned Kimi-K3.1-Pro checkpoint's own encoder.
//!
//! `tests/fixtures/kimi_k3/k31_serving_reference.json` records, for every case
//! group the gateway must serve, the text and the token IDs that the checkpoint
//! at revision `6d0a0202956796543364d666ab19c6ad1a494ee1` renders through
//! `encoding_k3.py::build_chat_segments` and
//! `tokenization_kimi.TikTokenTokenizer._encode_chat_segments`. The file is
//! committed, so a test reads no network and no checkpoint.
//!
//! `generate_k31_reference.py` in the same directory regenerates the file from
//! an explicit local snapshot of that revision. A case that does not match is a
//! renderer defect: fix the renderer, never the comparison. The comparison is
//! byte and ID equality, with no trim, no normalization and no JSON-equality
//! shortcut.
//!
//! The checkpoint's tokenizer files come from the K3.1 repository. The
//! `KIMI_K31_MODEL_DIR` environment variable points at a local checkpoint
//! directory; otherwise the tokenizer files are fetched once into
//! `.tokenizer_cache/kimi_k31/`, like the K3 helper.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::{fs, path::PathBuf};

use llm_tokenizer::{
    chat_template::ChatTemplateParams,
    traits::{PromptEncoding, Tokenizer as TokenizerTrait},
    TiktokenTokenizer,
};
use serde_json::Value;

use crate::common;

/// The revision the fixture was generated from.
const REFERENCE_REVISION: &str = "6d0a0202956796543364d666ab19c6ad1a494ee1";

fn fixture() -> Value {
    let raw = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/kimi_k3/k31_serving_reference.json"),
    )
    .expect("k31 serving reference must exist");
    serde_json::from_str(&raw).expect("k31 serving reference must be valid JSON")
}

/// The eight case groups the fixture must cover, with the minimum case count per
/// group. The plan names each group; this list keeps a regeneration from
/// silently dropping one.
const REQUIRED_GROUPS: &[(&str, usize)] = &[
    ("plain", 1),
    ("system", 1),
    ("multi_turn", 1),
    ("tools", 1),
    ("effort", 5),
    ("image", 1),
    ("developer", 1),
    ("tool_arguments", 1),
];

#[test]
fn fixture_records_the_pinned_revision_and_every_case_group() {
    let value = fixture();
    assert_eq!(
        value["revision"].as_str(),
        Some(REFERENCE_REVISION),
        "fixture must name the pinned checkpoint revision"
    );
    assert_eq!(
        value["repository"].as_str(),
        Some("moonshotai/Kimi-K3.1-Pro"),
        "fixture must name the checkpoint repository"
    );
    let cases = value["cases"].as_object().expect("cases must be an object");
    for (group, minimum) in REQUIRED_GROUPS {
        let count = cases
            .values()
            .filter(|case| case["group"].as_str() == Some(group))
            .count();
        assert!(
            count >= *minimum,
            "group `{group}` has {count} cases, needs at least {minimum}"
        );
    }
    for (name, case) in cases {
        if case.get("refusal").is_some() {
            assert!(
                case["refusal_message"].is_string(),
                "refusal case `{name}` must record the reference's message"
            );
            continue;
        }
        assert!(
            case["expected_text"].is_string(),
            "case `{name}` must record expected_text"
        );
        assert!(
            case["expected_token_ids"].is_array(),
            "case `{name}` must record expected_token_ids"
        );
        assert!(
            !case["expected_token_ids"]
                .as_array()
                .expect("ids")
                .is_empty(),
            "case `{name}` must record a nonempty token list"
        );
    }
}

/// Render one case through the tokenizer path the gateway uses and compare the
/// text and the token IDs. The encoder writes its own placeholder for an image
/// part, so a media case needs no extra input here.
fn assert_reference_match(tok: &TiktokenTokenizer, name: &str, case: &Value) {
    let messages: Vec<Value> = case["request"]["messages"]
        .as_array()
        .expect("messages")
        .clone();
    let tools: Option<Vec<Value>> = case["request"]["tools"].as_array().cloned();
    // `thinking` absent in the request means the gateway's default, which the
    // encoder reads as thinking on. `false` must reach the renderer, or the
    // effort header is written where the reference writes none.
    let thinking = case["request"]
        .get("thinking")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    // The request's `reasoning_effort` reaches the render as the template
    // variable the profile reads. An absent one leaves the served default,
    // which the reference recorded as `max`.
    let effort = case["request"]
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .unwrap_or("max")
        .to_string();
    let template_kwargs = std::collections::HashMap::from([(
        "reasoning_effort".to_string(),
        Value::String(effort),
    )]);
    let params = ChatTemplateParams {
        add_generation_prompt: case["request"]
            .get("add_generation_prompt")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        tools: tools.as_deref(),
        thinking: Some(thinking),
        template_kwargs: Some(&template_kwargs),
        ..Default::default()
    };
    let rendered = tok
        .apply_chat_template_with_encoding(&messages, params, None)
        .unwrap_or_else(|e| panic!("case `{name}`: render failed: {e}"));
    let expected_text = case["expected_text"].as_str().expect("expected_text");
    assert_eq!(
        rendered.text, expected_text,
        "case `{name}`: rendered bytes differ from the reference"
    );
    let expected_ids: Vec<u32> = case["expected_token_ids"]
        .as_array()
        .expect("ids")
        .iter()
        .map(|v| v.as_u64().expect("id") as u32)
        .collect();
    let PromptEncoding::Deferred(job) = rendered.encoding else {
        panic!("case `{name}`: K3.1 must defer its encode");
    };
    let ids = job
        .run()
        .unwrap_or_else(|e| panic!("case `{name}`: deferred encode failed: {e}"));
    assert_eq!(
        ids.token_ids(),
        &expected_ids[..],
        "case `{name}`: token IDs differ from the reference"
    );
}

/// One case group: every case the gateway serves, non-media.
#[test]
fn k31_binding_cases_match_the_reference_bytes_and_ids() {
    let dir = common::ensure_kimi_k31_cached();
    let tok = TiktokenTokenizer::from_dir(&dir).expect("K3.1 tokenizer should load");
    let value = fixture();
    let mut ran = 0usize;
    for (name, case) in value["cases"].as_object().expect("cases") {
        if case["transport"].as_str() != Some("binding") {
            continue;
        }
        // The refusal case records the reference's error, not bytes.
        if case.get("refusal").is_some() {
            continue;
        }
        assert_reference_match(&tok, name, case);
        ran += 1;
    }
    assert!(
        ran >= 12,
        "expected at least 12 binding cases, ran {ran}"
    );
}

/// The reference refuses an effort outside its accepted set before it writes a
/// segment. The renderer must refuse the same value, so a caller never sends a
/// prompt the checkpoint's own encoder would not build.
#[test]
fn k31_invalid_effort_is_refused_as_the_reference_refuses_it() {
    let dir = common::ensure_kimi_k31_cached();
    let tok = TiktokenTokenizer::from_dir(&dir).expect("K3.1 tokenizer should load");
    let value = fixture();
    let case = value["cases"]
        .get("effort_invalid_is_refused")
        .expect("effort_invalid_is_refused case");
    assert_eq!(
        case["refusal"].as_str(),
        Some("AssertionError"),
        "the reference refuses through an assertion"
    );
    let messages: Vec<Value> = case["request"]["messages"].as_array().expect("messages").clone();
    // The strict channel is `chat_template_kwargs.thinking_effort`: the
    // profile drops an unsupported top-level `reasoning_effort` before the
    // render, as the gateway does, but an explicit effort is validated as the
    // reference validates it.
    let kwargs = std::collections::HashMap::from([(
        "thinking_effort".to_string(),
        Value::String("minimal".to_string()),
    )]);
    let params = ChatTemplateParams {
        add_generation_prompt: true,
        thinking: Some(true),
        template_kwargs: Some(&kwargs),
        ..Default::default()
    };
    let error = tok
        .apply_chat_template(&messages, params)
        .expect_err("minimal effort must be refused");
    let message = format!("{error}");
    assert!(
        message.contains("minimal"),
        "the refusal must name the value: {message}"
    );
}

/// The image case through the same tokenizer path. The public binding refuses
/// media content, so this case proves the lower-level path the gRPC serving
/// uses. The reference rendered the user segment with one `<|media_pad|>`
/// prompt in place of the placeholder the encoder writes.
#[test]
fn k31_image_case_matches_the_reference_bytes_and_ids() {
    let dir = common::ensure_kimi_k31_cached();
    let tok = TiktokenTokenizer::from_dir(&dir).expect("K3.1 tokenizer should load");
    let value = fixture();
    let case = value["cases"]
        .get("image_part")
        .expect("image_part case");
    assert_eq!(case["transport"].as_str(), Some("media"));
    assert_reference_match(&tok, "image_part", case);
}

/// A checkpoint directory that names `model_type=kimi_k31` selects the K3.1
/// renderer, whose accepted `medium` effort reaches the prompt. This is the
/// renderer-resolution half of the proof the reference renders bind: the bytes
/// above come from the K3.1 encoder, and this case shows the tokenizer selects
/// that encoder rather than the K3 one.
#[test]
fn k31_model_type_selects_the_k31_renderer() {
    let dir = common::ensure_kimi_k31_cached();
    let tok = TiktokenTokenizer::from_dir(&dir).expect("K3.1 tokenizer should load");
    let messages = vec![serde_json::json!({"role": "user", "content": "Hi"})];
    let params = ChatTemplateParams {
        add_generation_prompt: true,
        ..Default::default()
    };
    let rendered = tok
        .apply_chat_template(&messages, params)
        .expect("render should succeed");
    // K3 (no `medium`) writes `thinking_effort=medium` only through the K3.1
    // profile, so a `medium` request that renders proves the profile.
    let params = ChatTemplateParams {
        add_generation_prompt: true,
        template_kwargs: Some(&std::collections::HashMap::from([(
            "thinking_effort".to_string(),
            serde_json::json!("medium"),
        )])),
        ..Default::default()
    };
    let medium = tok
        .apply_chat_template(&messages, params)
        .expect("K3.1 accepts medium effort");
    assert!(
        medium.contains("thinking_effort=medium"),
        "K3.1 must accept `medium`: {medium}"
    );
    assert!(
        !rendered.contains("thinking_effort=medium"),
        "the default render writes no medium effort: {rendered}"
    );
}