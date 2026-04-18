use openai_protocol::skills::{ResponsesSkillEntry, ResponsesSkillRef, SkillVersionRef};
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize)]
struct OptionalVersionHolder {
    version: Option<SkillVersionRef>,
}

#[test]
fn skill_version_ref_deserializes_latest() {
    let parsed: SkillVersionRef = serde_json::from_value(json!("latest")).unwrap();
    assert_eq!(parsed, SkillVersionRef::Latest);
}

#[test]
fn skill_version_ref_deserializes_integer_from_number() {
    let parsed: SkillVersionRef = serde_json::from_value(json!(2)).unwrap();
    assert_eq!(parsed, SkillVersionRef::Integer(2));
}

#[test]
fn skill_version_ref_rejects_ambiguous_numeric_string() {
    let err = serde_json::from_value::<SkillVersionRef>(json!("2")).unwrap_err();
    assert!(err
        .to_string()
        .contains("use a JSON number for integer versions"));
}

#[test]
fn skill_version_ref_deserializes_timestamp_string() {
    let parsed: SkillVersionRef = serde_json::from_value(json!("1759178010641129")).unwrap();
    assert_eq!(
        parsed,
        SkillVersionRef::Timestamp("1759178010641129".to_string())
    );
}

#[test]
fn skill_version_ref_rejects_unknown_string() {
    let err = serde_json::from_value::<SkillVersionRef>(json!("some-other-string")).unwrap_err();
    assert!(err.to_string().contains("invalid skill version string"));
}

#[test]
fn optional_skill_version_ref_accepts_null_and_absent() {
    let null_value: OptionalVersionHolder =
        serde_json::from_value(json!({"version": null})).unwrap();
    assert_eq!(null_value.version, None);

    let absent_value: OptionalVersionHolder = serde_json::from_value(json!({})).unwrap();
    assert_eq!(absent_value.version, None);
}

#[test]
fn responses_skill_entry_deserializes_typed_reference() {
    let raw = json!({
        "type": "skill_reference",
        "skill_id": "skill_123",
        "version": "latest"
    });

    let parsed: ResponsesSkillEntry = serde_json::from_value(raw.clone()).unwrap();
    assert_eq!(
        parsed,
        ResponsesSkillEntry::Typed(ResponsesSkillRef::Reference {
            skill_id: "skill_123".to_string(),
            version: Some(SkillVersionRef::Latest),
        })
    );
    assert_eq!(serde_json::to_value(&parsed).unwrap(), raw);
}

#[test]
fn responses_skill_entry_round_trips_opaque_openai_entry() {
    let raw = json!({
        "type": "inline_skill",
        "name": "map",
        "description": "Map the codebase",
        "instructions": "Read the crate map before implementing changes."
    });

    let parsed: ResponsesSkillEntry = serde_json::from_value(raw.clone()).unwrap();
    assert_eq!(parsed, ResponsesSkillEntry::OpaqueOpenAI(raw.clone()));
    assert_eq!(serde_json::to_value(&parsed).unwrap(), raw);
}

#[test]
fn responses_skill_entry_rejects_malformed_typed_reference() {
    let err = serde_json::from_value::<ResponsesSkillEntry>(json!({
        "type": "skill_reference"
    }))
    .unwrap_err();

    assert!(err.to_string().contains("missing field `skill_id`"));
}

#[test]
fn responses_skill_entry_rejects_non_object_payloads() {
    for raw in [json!(null), json!("inline_skill"), json!(["inline_skill"])] {
        let err = serde_json::from_value::<ResponsesSkillEntry>(raw).unwrap_err();
        assert!(err
            .to_string()
            .contains("responses skill entries must be JSON objects"));
    }
}
