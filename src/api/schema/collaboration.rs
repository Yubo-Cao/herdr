use serde::{Deserialize, Serialize};

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CollaborationRole {
    Owner,
    #[default]
    Editor,
    Viewer,
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CollaborationActivity {
    #[default]
    Active,
    Idle,
    Away,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CollaborationParticipant {
    pub participant_id: String,
    pub display_name: String,
    pub color: String,
    pub role: CollaborationRole,
    pub activity: CollaborationActivity,
    pub surface: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub typing: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub typing_expires_at_unix_ms: Option<u64>,
    pub updated_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CollaborationPaneClaim {
    pub pane_id: String,
    pub participant_id: String,
    pub acquired_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protected_until_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CollaborationSnapshot {
    pub participants: Vec<CollaborationParticipant>,
    pub pane_claims: Vec<CollaborationPaneClaim>,
    pub lease_ttl_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CollaborationUpdateParams {
    pub participant_id: String,
    pub display_name: String,
    pub color: String,
    #[serde(default)]
    pub role: CollaborationRole,
    #[serde(default)]
    pub activity: CollaborationActivity,
    #[serde(default = "default_surface")]
    pub surface: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    #[serde(default)]
    pub typing: bool,
}

fn default_surface() -> String {
    "api".to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CollaborationLeaveParams {
    pub participant_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CollaborationClaimParams {
    pub participant_id: String,
    pub pane_id: String,
    #[serde(default)]
    pub takeover: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protect_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CollaborationReleaseParams {
    pub participant_id: String,
    pub pane_id: String,
}
