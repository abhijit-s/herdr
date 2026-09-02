use serde::{Deserialize, Serialize};

/// Push a source-keyed value into the status strip's push lane. The value
/// renders wherever a matching `#{slot:NAME}` token appears in `status_right`.
/// Host-scoped (keyed by `source` only). Last-writer-wins by `seq`; `ttl_ms`
/// expires the value lazily.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StatusSetParams {
    pub source: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u64>,
}

/// Clear the pushed value for a source, emptying any `#{slot:NAME}` bound to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StatusClearParams {
    pub source: String,
}
