//! Stable, owned host contracts. No Discord, runtime, SQL or credential types.
#![forbid(unsafe_code)]
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidInput,
    ModuleUnavailable,
    Compatibility,
    DependencyUnavailable,
    DataVersionMismatch,
    SchemaInvalid,
    QuotaExceeded,
    TrustedCodeRequired,
    ArtifactChanged,
    ForbiddenScope,
    ForbiddenPermission,
    Conflict,
    NotFound,
    StorageUnavailable,
    MigrationMismatch,
    Backup,
    Integrity,
    AlreadyRunning,
    Cancelled,
    UnknownOutcome,
    RecoveryRequired,
    Io,
}
pub struct Error {
    pub code: ErrorCode,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}
impl Error {
    pub fn new(code: ErrorCode) -> Self {
        Self { code, source: None }
    }
    pub fn with_source(
        code: ErrorCode,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            code,
            source: Some(Box::new(source)),
        }
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.code)
    }
}
impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_deref().map(|s| s as _)
    }
}
pub type Result<T> = std::result::Result<T, Error>;
fn snowflake(s: &str) -> bool {
    !s.starts_with('0')
        && s.bytes().all(|b| b.is_ascii_digit())
        && s.parse::<u64>().is_ok_and(|id| id > 0)
}
fn uuid_id(s: &str) -> bool {
    uuid::Uuid::parse_str(s).is_ok_and(|id| id.to_string() == s)
}
macro_rules! identifier {
    ($name:ident,$validate:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);
        impl $name {
            pub fn new(s: impl Into<String>) -> Result<Self> {
                let s = s.into();
                if $validate(&s) {
                    Ok(Self(s))
                } else {
                    Err(Error::new(ErrorCode::InvalidInput))
                }
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl std::str::FromStr for $name {
            type Err = Error;
            fn from_str(s: &str) -> Result<Self> {
                Self::new(s)
            }
        }
        impl TryFrom<String> for $name {
            type Error = Error;
            fn try_from(s: String) -> Result<Self> {
                Self::new(s)
            }
        }
        impl From<$name> for String {
            fn from(id: $name) -> String {
                id.0
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}
identifier!(GuildId, snowflake);
identifier!(UserId, snowflake);
identifier!(DeploymentId, uuid_id);
identifier!(OperationId, uuid_id);
identifier!(EffectId, uuid_id);
impl DeploymentId {
    pub fn generate() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}
impl OperationId {
    pub fn generate() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}
impl EffectId {
    pub fn generate() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EffectState {
    Prepared,
    Sent,
    Verified,
    Unknown,
    Failed,
}
impl EffectState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Sent => "sent",
            Self::Verified => "verified",
            Self::Unknown => "unknown",
            Self::Failed => "failed",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OperationState {
    Running,
    Succeeded,
    RecoveryRequired,
    Failed,
}
impl OperationState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::RecoveryRequired => "recovery_required",
            Self::Failed => "failed",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GuildState {
    pub guild: GuildId,
    pub paused: bool,
    pub revision: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Operation {
    pub id: OperationId,
    pub guild: GuildId,
    pub actor: String,
    pub state: OperationState,
    pub revision: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Effect {
    pub id: EffectId,
    pub operation: OperationId,
    pub guild: GuildId,
    pub purpose: String,
    pub state: EffectState,
    pub revision: u64,
    pub receipt: Option<Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Status {
    pub deployment: DeploymentId,
    pub guilds: Vec<GuildState>,
    pub modules_loaded: usize,
    pub recovery_required: u64,
    pub ai_available: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ControlReceipt {
    pub operation: OperationId,
    pub guild: GuildState,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn untrusted_identifiers_are_validated_during_deserialization() {
        for bad in ["", "0", "01", "-1", "1 OR 1=1", "18446744073709551616"] {
            assert!(GuildId::new(bad).is_err());
            assert!(serde_json::from_value::<GuildId>(Value::String(bad.into())).is_err());
        }
        assert!(GuildId::new("18446744073709551615").is_ok());
        assert!(DeploymentId::new("not-a-uuid").is_err());
        let id = DeploymentId::generate();
        assert_eq!(
            serde_json::from_value::<DeploymentId>(serde_json::to_value(&id).unwrap()).unwrap(),
            id
        );
    }
    #[test]
    fn error_display_does_not_expose_internal_source() {
        let error = Error::with_source(
            ErrorCode::StorageUnavailable,
            std::io::Error::other("password=private"),
        );
        assert_eq!(format!("{error}"), "StorageUnavailable");
        assert_eq!(format!("{error:?}"), "StorageUnavailable");
        assert!(std::error::Error::source(&error).is_some());
    }
}

fn module_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && !s.ends_with('.')
        && !s.contains("..")
}
identifier!(ModuleId, module_id);
pub mod modules;
pub use modules::*;
