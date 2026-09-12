//! Executable Stage 0 contract sketch, not a production authorization service.
//! All identities and policies here must be supplied by authenticated host code.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

type Result<T> = std::result::Result<T, &'static str>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub guild: String,
    pub module: String,
    pub session: String,
    pub generation: u64,
    pub epoch: u64,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Audience {
    #[default]
    Operator,
    MemberRead,
}
#[derive(Clone, Debug)]
pub struct Actor {
    pub guild: String,
    pub channel: String,
    pub roles: BTreeSet<String>,
    pub manage_guild: bool,
    pub listed_operator: bool,
}
#[derive(Clone, Debug)]
pub struct State {
    pub identity: Identity,
    pub active: bool,
    pub paused: bool,
    pub policy_revision: u64,
    pub channels: BTreeSet<String>,
    pub roles: BTreeSet<String>,
    pub quota_available: bool,
}
#[derive(Clone, Debug)]
pub struct Operation {
    pub name: String,
    pub audience: Audience,
    pub capabilities: BTreeSet<String>,
}
#[derive(Clone, Debug)]
pub struct Lease {
    identity: Identity,
    actor: Actor,
    policy_revision: u64,
    audience: Audience,
    capabilities: BTreeSet<String>,
    live: bool,
}
impl Lease {
    pub fn finish(&mut self) {
        self.live = false;
    }
    pub fn audience(&self) -> Audience {
        self.audience
    }
}
fn current(actor: &Actor, binding: &Identity, state: &State) -> Result<()> {
    if binding != &state.identity || actor.guild != state.identity.guild {
        return Err("stale or cross-guild identity");
    }
    if !state.active || state.paused {
        return Err("inactive or paused");
    }
    Ok(())
}
fn member_policy(actor: &Actor, state: &State) -> Result<()> {
    if (!state.channels.is_empty() && !state.channels.contains(&actor.channel))
        || (!state.roles.is_empty() && actor.roles.is_disjoint(&state.roles))
    {
        return Err("member policy");
    }
    Ok(())
}
/// Member operations carry no callback grants, even when invoked by an operator.
/// Quota consumption belongs in the host's atomic admission transaction.
pub fn admit(
    actor: &Actor,
    binding: &Identity,
    operation: &Operation,
    state: &State,
) -> Result<Lease> {
    current(actor, binding, state)?;
    if operation.audience == Audience::MemberRead {
        if !operation.capabilities.is_empty() {
            return Err("member callback capabilities forbidden");
        }
        member_policy(actor, state)?;
        if !state.quota_available {
            return Err("quota");
        }
    } else if !actor.manage_guild || !actor.listed_operator {
        return Err("operator required");
    }
    Ok(Lease {
        identity: binding.clone(),
        actor: actor.clone(),
        policy_revision: state.policy_revision,
        audience: operation.audience,
        capabilities: operation.capabilities.clone(),
        live: true,
    })
}
pub fn callback(lease: &Lease, method: &str, state: &State) -> Result<()> {
    current(&lease.actor, &lease.identity, state)?;
    if !lease.live || lease.policy_revision != state.policy_revision {
        return Err("revoked lease");
    }
    // The public corpus needs no host callback. Deny all now; do not grant
    // storage.own, which currently grants both reads and writes.
    if lease.audience == Audience::MemberRead {
        return Err("member callback denied");
    }
    let required: &[&str] = match method {
        "host.document_get" | "host.document_batch" => &["storage.own"],
        "host.contract_invoke" => &["contracts.invoke"],
        "host.notify" => &["discord.notify"],
        "host.echo" => &["host.echo"],
        "host.health" => &["config.own", "events.guild"],
        _ => return Err("unknown callback"),
    };
    if required.iter().all(|cap| lease.capabilities.contains(*cap)) {
        Ok(())
    } else {
        Err("capability denied")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TypedOption {
    pub name: String,
    pub required: bool,
    pub kind: OptionKind,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum OptionKind {
    String {
        max_length: usize,
        choices: Vec<String>,
    },
    Integer {
        minimum: i64,
        maximum: i64,
    },
    Boolean,
}
/// Mapping is explicit and flat: option names are input property names. The
/// integration must also validate the final object against input_schema.
pub fn typed_input(options: &[TypedOption], input: &Value) -> Result<Value> {
    let object = input.as_object().ok_or("object required")?;
    let mut names = BTreeSet::new();
    if options.len() > 25 {
        return Err("too many options");
    }
    for option in options {
        if !names.insert(&option.name) {
            return Err("duplicate descriptor");
        }
        let Some(value) = object.get(&option.name) else {
            if option.required {
                return Err("required option");
            }
            continue;
        };
        let valid = match &option.kind {
            OptionKind::String {
                max_length,
                choices,
            } => value.as_str().is_some_and(|s| {
                !s.is_empty()
                    && s.chars().count() <= *max_length
                    && (choices.is_empty() || choices.iter().any(|choice| choice == s))
            }),
            OptionKind::Integer { minimum, maximum } => value
                .as_i64()
                .is_some_and(|n| n >= *minimum && n <= *maximum),
            OptionKind::Boolean => value.is_boolean(),
        };
        if !valid {
            return Err("option type or range");
        }
    }
    if object.keys().any(|key| !names.contains(key)) {
        return Err("unknown option");
    }
    Ok(input.clone())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplyV2 {
    pub text: String,
    pub citations: Vec<String>,
}
/// Host-owned interaction response, no arbitrary channel destination or embeds.
/// Public text is inert plain text; suppress Discord mentions and link previews.
pub fn render(reply: &ReplyV2, approved_citation_prefix: &str) -> Result<Value> {
    if reply.text.is_empty() || reply.citations.len() > 5 {
        return Err("reply bounds");
    }
    if reply.text.chars().any(|c| c.is_control() && c != '\n') {
        return Err("control text");
    }
    // Canonical revision URLs are composed from validated numeric IDs, not
    // arbitrary wiki-authored links or module-selected destinations.
    if reply.citations.iter().any(|s| {
        !s.strip_prefix(approved_citation_prefix)
            .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
    }) {
        return Err("citation origin or revision");
    }
    let text = reply
        .text
        .replace('@', "＠")
        .replace(['<', '>'], "")
        .replace(['*', '_', '`', '~', '|', '[', ']'], "");
    let content = if reply.citations.is_empty() {
        text
    } else {
        format!("{}\nSources:\n{}", text, reply.citations.join("\n"))
    };
    if content.encode_utf16().count() > 1800 {
        return Err("reply too long");
    }
    Ok(json!({"content": content,"allowed_mentions":{"parse":[]},"flags":68}))
}

/// Operator creates and owns both directories before initialization. Canonical
/// resolution rejects symlink escapes into the immutable package. This is not
/// a filesystem sandbox: the native module is trusted.
pub fn data_directory(configured: &Path, package: &Path) -> Result<PathBuf> {
    if !configured.is_absolute() || !package.is_absolute() {
        return Err("absolute path required");
    }
    let data = configured
        .canonicalize()
        .map_err(|_| "missing data directory")?;
    let package = package.canonicalize().map_err(|_| "missing package")?;
    if !data.is_dir()
        || !package.is_dir()
        || data.starts_with(&package)
        || package.starts_with(&data)
    {
        return Err("overlapping package and data directories");
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup() -> (Actor, State, Operation) {
        let actor = Actor {
            guild: "123".into(),
            channel: "456".into(),
            roles: BTreeSet::from(["player".into()]),
            manage_guild: false,
            listed_operator: false,
        };
        let state = State {
            identity: Identity {
                guild: "123".into(),
                module: "dw".into(),
                session: "boot-a".into(),
                generation: 2,
                epoch: 3,
            },
            active: true,
            paused: false,
            policy_revision: 1,
            channels: BTreeSet::from(["456".into()]),
            roles: BTreeSet::from(["player".into()]),
            quota_available: true,
        };
        let operation = Operation {
            name: "lookup".into(),
            audience: Audience::MemberRead,
            capabilities: BTreeSet::new(),
        };
        (actor, state, operation)
    }
    #[test]
    fn member_read_never_acquires_callback_authority() {
        let (mut actor, state, mut op) = setup();
        for admin in [false, true] {
            actor.manage_guild = admin;
            actor.listed_operator = admin;
            let lease = admit(&actor, &state.identity, &op, &state).unwrap();
            for method in [
                "host.document_get",
                "host.document_batch",
                "host.notify",
                "host.echo",
                "host.contract_invoke",
                "host.health",
                "host.config_set",
                "host.refresh",
            ] {
                assert!(callback(&lease, method, &state).is_err(), "{method}");
            }
        }
        op.capabilities.insert("storage.own".into());
        assert!(admit(&actor, &state.identity, &op, &state).is_err());
    }
    #[test]
    fn forged_stale_and_revoked_identities_fail() {
        let (actor, state, op) = setup();
        for index in 0..5 {
            let mut forged = state.identity.clone();
            match index {
                0 => forged.guild = "999".into(),
                1 => forged.session = "old".into(),
                2 => forged.generation += 1,
                3 => forged.epoch += 1,
                _ => forged.module = "other".into(),
            }
            assert!(admit(&actor, &forged, &op, &state).is_err());
        }
        let mut foreign = actor.clone();
        foreign.guild = "999".into();
        assert!(admit(&foreign, &state.identity, &op, &state).is_err());
        let mut admin = actor.clone();
        admin.manage_guild = true;
        admin.listed_operator = true;
        let admin_op = Operation {
            name: "write".into(),
            audience: Audience::Operator,
            capabilities: BTreeSet::from(["storage.own".into()]),
        };
        let mut lease = admit(&admin, &state.identity, &admin_op, &state).unwrap();
        assert!(callback(&lease, "host.document_batch", &state).is_ok());
        for index in 0..5 {
            let mut changed = state.clone();
            match index {
                0 => changed.identity.guild = "999".into(),
                1 => changed.identity.session = "new".into(),
                2 => changed.identity.generation += 1,
                3 => changed.identity.epoch += 1,
                _ => changed.policy_revision += 1,
            }
            assert!(callback(&lease, "host.document_batch", &changed).is_err());
        }
        lease.finish();
        assert!(callback(&lease, "host.document_batch", &state).is_err());
    }
    #[test]
    fn member_policy_pause_activation_and_quota_are_enforced() {
        let (actor, state, op) = setup();
        for index in 0..5 {
            let mut denied = state.clone();
            match index {
                0 => denied.active = false,
                1 => denied.paused = true,
                2 => denied.quota_available = false,
                3 => denied.channels = BTreeSet::from(["other".into()]),
                _ => denied.roles = BTreeSet::from(["other".into()]),
            }
            assert!(admit(&actor, &state.identity, &op, &denied).is_err());
        }
    }
    #[test]
    fn operator_default_requires_both_checks_and_retains_grants() {
        let (mut actor, state, mut op) = setup();
        op.audience = Audience::default();
        op.capabilities.insert("storage.own".into());
        for (manage, listed) in [(false, false), (false, true), (true, false)] {
            actor.manage_guild = manage;
            actor.listed_operator = listed;
            assert!(admit(&actor, &state.identity, &op, &state).is_err());
        }
        actor.manage_guild = true;
        actor.listed_operator = true;
        let lease = admit(&actor, &state.identity, &op, &state).unwrap();
        assert!(callback(&lease, "host.document_get", &state).is_ok());
        assert!(callback(&lease, "host.document_batch", &state).is_ok());
        assert!(callback(&lease, "host.notify", &state).is_err());
    }
    #[test]
    fn typed_options_reject_coercion_and_unknown_keys() {
        let options = vec![
            TypedOption {
                name: "name".into(),
                required: true,
                kind: OptionKind::String {
                    max_length: 80,
                    choices: vec![],
                },
            },
            TypedOption {
                name: "floor".into(),
                required: false,
                kind: OptionKind::Integer {
                    minimum: 1,
                    maximum: 1000,
                },
            },
            TypedOption {
                name: "spoilers".into(),
                required: false,
                kind: OptionKind::Boolean,
            },
            TypedOption {
                name: "category".into(),
                required: false,
                kind: OptionKind::String {
                    max_length: 20,
                    choices: vec!["toon".into(), "twisted".into()],
                },
            },
        ];
        let input = json!({"name":"Pebble","floor":20,"spoilers":false,"category":"toon"});
        assert_eq!(typed_input(&options, &input).unwrap(), input);
        for bad in [
            json!({}),
            json!({"name":false}),
            json!({"name":"Pebble","floor":"20"}),
            json!({"name":"Pebble","floor":0}),
            json!({"name":"Pebble","floor":1.5}),
            json!({"name":"Pebble","spoilers":"false"}),
            json!({"name":"Pebble","category":"admin"}),
            json!({"name":"Pebble","refresh":true}),
            json!({"name":"x".repeat(81)}),
        ] {
            assert!(typed_input(&options, &bad).is_err(), "{bad}");
        }
    }
    #[test]
    fn actual_v1_contract_rejects_proposed_fields() {
        let base = json!({"name":"lookup","description":"Look up a topic","operation":"lookup","input_required":true});
        assert!(
            serde_json::from_value::<oracle_contracts::ModuleCommandRoute>(base.clone()).is_ok()
        );
        let mut extended = base;
        extended["options"] = json!([]);
        assert!(serde_json::from_value::<oracle_contracts::ModuleCommandRoute>(extended).is_err());
        let mut manifest: Value = serde_json::from_str(include_str!(
            "../../../examples/modules/counter/manifest.json"
        ))
        .unwrap();
        assert!(
            serde_json::from_value::<oracle_contracts::ModuleManifest>(manifest.clone()).is_ok()
        );
        manifest["data_directory"] = json!("/tmp/dw");
        assert!(serde_json::from_value::<oracle_contracts::ModuleManifest>(manifest).is_err());
    }
    #[test]
    fn readable_reply_has_no_transport_control_or_mentions() {
        let prefix = "https://dandys-world-robloxhorror.fandom.com/wiki/?oldid=";
        let reply = ReplyV2 {
            text: "**Pebble** @everyone <@123>".into(),
            citations: vec!["https://dandys-world-robloxhorror.fandom.com/wiki/?oldid=123".into()],
        };
        let output = render(&reply, prefix).unwrap();
        assert_eq!(output["allowed_mentions"], json!({"parse":[]}));
        assert_eq!(output["flags"], 68);
        assert!(
            output["content"]
                .as_str()
                .unwrap()
                .starts_with("Pebble ＠everyone ＠123")
        );
        assert!(
            render(
                &ReplyV2 {
                    text: "😀".repeat(901),
                    citations: vec![]
                },
                prefix
            )
            .is_err()
        );
        assert!(
            render(
                &ReplyV2 {
                    text: "fact".into(),
                    citations: vec!["https://evil.invalid/wiki/?oldid=123".into()]
                },
                prefix
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<ReplyV2>(
                json!({"text":"fact","citations":[],"destination":"999"})
            )
            .is_err()
        );
    }
    #[test]
    fn data_path_is_explicit_external_and_symlink_checked() {
        let root = std::env::temp_dir().join(format!("dw-host-seams-{}", std::process::id()));
        let package = root.join("installed");
        let data = root.join("data");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        assert_eq!(
            data_directory(&data, &package).unwrap(),
            data.canonicalize().unwrap()
        );
        assert!(data_directory(Path::new("relative"), &package).is_err());
        assert!(data_directory(&package, &package).is_err());
        assert!(data_directory(&root, &package).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&package, root.join("escape")).unwrap();
            assert!(data_directory(&root.join("escape"), &package).is_err());
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
