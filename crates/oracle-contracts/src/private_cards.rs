//! Protocol 1.2 private controls. Inputs are data, never member authority.
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivateCardV2 {
    pub card: PrivateCardBody,
    #[serde(default)]
    pub choices: Vec<PrivateCardChoice>,
    #[serde(default)]
    pub buttons: Vec<PrivateCardButton>,
    #[serde(default)]
    pub select_placeholder: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivateCardBody {
    pub title: String,
    pub description: String,
    #[serde(default)]
    pub fields: Vec<PrivateCardField>,
    #[serde(default)]
    pub footer: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivateCardMember {
    pub user_id: crate::UserId,
    #[serde(default)]
    pub prefix: String,
    #[serde(default)]
    pub suffix: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivateCardField {
    pub name: String,
    pub value: String,
    pub inline: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<PrivateCardMember>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivateCardChoice {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_id: Option<crate::UserId>,
    pub label: String,
    #[serde(default)]
    pub description: String,
    pub operation: String,
    pub input: Map<String, Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivateCardButton {
    pub label: String,
    pub operation: String,
    pub input: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<PrivateCardPrompt>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivateCardPrompt {
    pub option: String,
    pub label: String,
    pub max_length: u16,
    #[serde(default)]
    pub placeholder: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub select: Option<PrivateCardPromptSelect>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_fields: Vec<PrivateCardPromptField>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivateCardPromptField {
    pub option: String,
    pub label: String,
    pub max_length: u16,
    #[serde(default)]
    pub placeholder: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivateCardPromptSelect {
    pub option: String,
    pub label: String,
    pub choices: Vec<PrivateCardPromptOption>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivateCardPromptOption {
    pub label: String,
    pub value: String,
}
