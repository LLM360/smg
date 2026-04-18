//! Skill protocol types shared across API surfaces.
//!
//! This module keeps wire-facing request fragments separate from the
//! request-local/internal types the router resolves after attach time.

use serde::{
    de::{self, Visitor},
    Deserialize, Deserializer, Serialize, Serializer,
};
use serde_json::Value;

/// Accepted in `/v1/messages` -> `container.skills[]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type")]
pub enum MessagesSkillRef {
    /// Anthropic-curated skill that SMG passes through unchanged.
    #[serde(rename = "anthropic")]
    Anthropic {
        skill_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        version: Option<String>,
    },
    /// SMG-managed skill resolved from storage.
    #[serde(rename = "custom")]
    Custom {
        skill_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        version: Option<SkillVersionRef>,
    },
}

/// Accepted in `/v1/responses` -> `tools[].environment.skills[]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type")]
pub enum ResponsesSkillRef {
    /// Hosted skill reference. Ownership is decided by storage lookup, not id shape.
    #[serde(rename = "skill_reference")]
    Reference {
        skill_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        version: Option<SkillVersionRef>,
    },
    /// Client-local path. SMG never resolves or inspects this payload.
    #[serde(rename = "local")]
    Local {
        name: String,
        description: String,
        path: String,
    },
}

/// One entry in `/v1/responses` -> `tools[].environment.skills[]`.
///
/// SMG interprets the typed subset directly and preserves all other JSON
/// object entries as opaque provider-owned payloads.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum ResponsesSkillEntry {
    Typed(ResponsesSkillRef),
    OpaqueOpenAI(Value),
}

impl<'de> Deserialize<'de> for ResponsesSkillEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;

        if !value.is_object() {
            return Err(de::Error::custom(
                "responses skill entries must be JSON objects",
            ));
        }

        if matches_typed_responses_skill_ref(&value) {
            return serde_json::from_value::<ResponsesSkillRef>(value)
                .map(Self::Typed)
                .map_err(de::Error::custom);
        }

        Ok(Self::OpaqueOpenAI(value))
    }
}

/// Accepted in `X-SMG-Skills` for `/v1/chat/completions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type")]
pub enum ChatCompletionsSkillRef {
    #[serde(rename = "custom")]
    Custom {
        skill_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        version: Option<SkillVersionRef>,
    },
    #[serde(rename = "openai")]
    OpenAI {
        skill_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        version: Option<String>,
    },
    #[serde(rename = "anthropic")]
    Anthropic {
        skill_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        version: Option<String>,
    },
}

/// Version reference accepted by skill attachment surfaces.
///
/// Deserialization is intentionally manual to reject ambiguous numeric strings
/// like `"2"` rather than guessing between integer and timestamp semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillVersionRef {
    /// The literal string `"latest"`.
    Latest,
    /// Integer reference (OpenAI-style).
    Integer(u32),
    /// Timestamp string (Anthropic-style).
    Timestamp(String),
}

impl Serialize for SkillVersionRef {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Latest => serializer.serialize_str("latest"),
            Self::Integer(version) => serializer.serialize_u32(*version),
            Self::Timestamp(version) => serializer.serialize_str(version),
        }
    }
}

impl<'de> Deserialize<'de> for SkillVersionRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct SkillVersionRefVisitor;

        impl<'de> Visitor<'de> for SkillVersionRefVisitor {
            type Value = SkillVersionRef;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(
                    "the string `latest`, an integer version, or a 10+ digit timestamp string",
                )
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                let value = u32::try_from(value).map_err(|_| {
                    E::custom(format!(
                        "skill version integer is out of range for u32: {value}"
                    ))
                })?;
                Ok(SkillVersionRef::Integer(value))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if value < 0 {
                    return Err(E::custom("skill version integer must be non-negative"));
                }
                self.visit_u64(value as u64)
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                parse_skill_version_str(value).map_err(E::custom)
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                self.visit_str(&value)
            }
        }

        deserializer.deserialize_any(SkillVersionRefVisitor)
    }
}

impl schemars::JsonSchema for SkillVersionRef {
    fn schema_name() -> String {
        "SkillVersionRef".to_string()
    }

    fn json_schema(r#gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
        SkillVersionRefWire::json_schema(r#gen)
    }
}

#[expect(
    dead_code,
    reason = "Skill resolution lands in follow-up PRs, but these internal protocol-side types stay crate-private now"
)]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ResolvedSkillRef {
    pub(crate) public_skill_id: String,
    pub(crate) origin: SkillOrigin,
    pub(crate) requested_version: Option<SkillVersionRef>,
    pub(crate) pinned: Option<PinnedSkillVersion>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PinnedSkillVersion {
    pub(crate) version: String,
    pub(crate) version_number: u32,
}

#[expect(
    dead_code,
    reason = "Skill resolution lands in follow-up PRs, but origin tracking belongs with the other crate-private resolver types"
)]
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SkillOrigin {
    AnthropicProvider {
        skill_id: String,
        raw_version: Option<String>,
    },
    OpenAIProvider {
        skill_id: String,
        raw_version: Option<String>,
    },
    OpenAIOpaquePassThrough {
        raw: Value,
    },
    SmgStorage {
        skill_id: String,
    },
    ClientLocalPath {
        name: String,
        description: String,
        path: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
enum SkillVersionRefWire {
    String(String),
    Integer(u32),
}

fn matches_typed_responses_skill_ref(value: &Value) -> bool {
    matches!(
        value.get("type").and_then(Value::as_str),
        Some("skill_reference" | "local")
    )
}

fn parse_skill_version_str(value: &str) -> Result<SkillVersionRef, String> {
    if value == "latest" {
        return Ok(SkillVersionRef::Latest);
    }

    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        if value.len() <= 9 {
            return Err(format!(
                "ambiguous skill version string `{value}`: use a JSON number for integer versions or a 10+ digit string for timestamps"
            ));
        }

        if value.bytes().all(|byte| byte == b'0') {
            return Err("timestamp skill version string must be a positive integer".to_string());
        }

        return Ok(SkillVersionRef::Timestamp(value.to_string()));
    }

    Err(format!(
        "invalid skill version string `{value}`: expected `latest` or a 10+ digit timestamp string"
    ))
}
