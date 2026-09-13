//! Configuration and startup boundaries for opt-in public module reads.
use super::*;
use crate::host::Host;
use oracle_core::{GuildId, ModuleId, member_read::MemberContext};
use oracle_storage::PgTools;
use serde_json::{Value, json};
use std::time::Instant;

fn scratch() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    root
}
fn legacy() -> Value {
    json!({"version":1,"state_dir":"state","database":{"backend":"sqlite","path":"state/oracle.sqlite"},"guilds":[{"guild":"100","operators":["200"]}],"discord":null})
}
fn policy() -> Value {
    json!({"guild":"100","module":"community.dandys-world","policy":{"channels":["300"],"roles":["400"],"per_user_per_minute":10,"per_guild_per_minute":60}})
}
fn load(root: &Path, value: &Value) -> Result<Config> {
    let path = root.join("oracle.json");
    std::fs::write(&path, serde_json::to_vec(value).unwrap()).unwrap();
    Config::load(&path)
}
fn context() -> MemberContext {
    MemberContext {
        guild: GuildId::new("100").unwrap(),
        user: oracle_core::UserId::new("201").unwrap(),
        channel: "300".into(),
        roles: std::collections::BTreeSet::from(["400".into()]),
        observed_at: Instant::now(),
    }
}
#[test]
fn legacy_configuration_keeps_opt_in_defaults_and_relative_resolution() {
    let root = scratch();
    let config = load(root.path(), &legacy()).unwrap();
    assert!(config.module_runtime.is_empty());
    assert!(config.member_reads.is_empty());
    assert_eq!(config.state_dir, root.path().join("state"));
    assert_eq!(config.source_path, Some(root.path().join("oracle.json")));
    let encoded = serde_json::to_value(&config).unwrap();
    assert!(encoded.get("module_runtime").is_none());
    assert!(encoded.get("member_reads").is_none());
    assert!(encoded.get("source_path").is_none());
}
#[test]
fn initialized_configuration_does_not_enable_public_routes() {
    let root = scratch();
    let path = root.path().join("oracle.json");
    initialize(&path, None).unwrap();
    let config = Config::load(&path).unwrap();
    assert!(config.module_runtime.is_empty() && config.member_reads.is_empty());
    assert!(initialize(&path, None).is_err());
}
#[test]
fn member_scope_duplicates_unknown_guilds_and_invalid_ids_are_rejected() {
    let root = scratch();
    let mut value = legacy();
    value["member_reads"] = json!([policy(), policy()]);
    assert!(load(root.path(), &value).is_err());
    for field in ["guild", "module"] {
        let mut entry = policy();
        entry[field] = json!("bad id");
        value["member_reads"] = json!([entry]);
        assert!(load(root.path(), &value).is_err());
    }
    let mut entry = policy();
    entry["guild"] = json!("999");
    value["member_reads"] = json!([entry]);
    assert!(load(root.path(), &value).is_err());
}
#[test]
fn guild_policy_duplicates_and_invalid_guild_ids_stay_rejected() {
    let root = scratch();
    for guilds in [
        json!([{"guild":"100","operators":[]},{"guild":"100","operators":[]}]),
        json!([{"guild":"not-a-guild","operators":[]}]),
        json!([{"guild":"100","operators":[],"member_reads":true}]),
    ] {
        let mut value = legacy();
        value["guilds"] = guilds;
        assert!(load(root.path(), &value).is_err());
    }
}
#[test]
fn newly_added_configuration_objects_reject_unknown_fields() {
    let root = scratch();
    let mut cases = Vec::new();
    let mut value = legacy();
    value["unknown"] = json!(true);
    cases.push(value);
    let mut value = legacy();
    let mut entry = policy();
    entry["audience"] = json!("operator");
    value["member_reads"] = json!([entry]);
    cases.push(value);
    let mut value = legacy();
    let mut entry = policy();
    entry["policy"]["allow_admin"] = json!(true);
    value["member_reads"] = json!([entry]);
    cases.push(value);
    let mut value = legacy();
    value["module_runtime"] = json!({"community.dandys-world":{"environment":{"TOKEN":"fixture"}}});
    cases.push(value);
    let mut value = legacy();
    value["member_reads"] = json!([{"guild":"100","module":"community.dandys-world"}]);
    cases.push(value);
    for case in cases {
        assert!(load(root.path(), &case).is_err());
    }
}
#[test]
fn configuration_limits_are_enforced_before_startup() {
    let root = scratch();
    let mut value = legacy();
    value["member_reads"] = json!(vec![policy(); 1025]);
    assert!(load(root.path(), &value).is_err());
    let entries = (0..129)
        .map(|n| (format!("module-{n}"), json!({})))
        .collect::<serde_json::Map<_, _>>();
    let mut value = legacy();
    value["module_runtime"] = Value::Object(entries);
    assert!(load(root.path(), &value).is_err());
}
#[tokio::test]
async fn host_defaults_do_not_grant_member_access_and_explicit_policy_is_installed() {
    let root = scratch();
    let config = load(root.path(), &legacy()).unwrap();
    let host = Host::open(&config, PgTools::default()).await.unwrap();
    let member = context();
    let module = ModuleId::new("community.dandys-world").unwrap();
    assert!(
        host.core
            .member_read_gate()
            .check_access(&member, &member.guild, &module)
            .is_err()
    );
    host.close().await.unwrap();
    drop(host);
    let mut value = legacy();
    value["member_reads"] = json!([policy()]);
    let config = load(root.path(), &value).unwrap();
    let host = Host::open(&config, PgTools::default()).await.unwrap();
    let member = context();
    host.core
        .member_read_gate()
        .check_access(&member, &member.guild, &module)
        .unwrap();
    host.close().await.unwrap();
}
#[tokio::test]
async fn invalid_policy_fails_startup_and_releases_host_lock() {
    for (field, value) in [
        ("per_user_per_minute", json!(0)),
        ("per_guild_per_minute", json!(601)),
        ("per_guild_per_minute", json!(1)),
        ("roles", json!(["bad-role"])),
        ("channels", json!(["0"])),
    ] {
        let root = scratch();
        let mut entry = policy();
        entry["policy"][field] = value;
        let mut value = legacy();
        value["member_reads"] = json!([entry]);
        let config = load(root.path(), &value).unwrap();
        assert!(Host::open(&config, PgTools::default()).await.is_err());
        let config = load(root.path(), &legacy()).unwrap();
        Host::open(&config, PgTools::default())
            .await
            .unwrap()
            .close()
            .await
            .unwrap();
    }
}
#[tokio::test]
async fn runtime_paths_reject_relative_and_host_owned_locations_without_activating_modules() {
    for case in ["relative", "database", "configuration", "package"] {
        let root = scratch();
        let path = match case {
            "relative" => PathBuf::from("data"),
            "database" => root.path().join("state/oracle.sqlite"),
            "configuration" => root.path().join("oracle.json"),
            _ => root.path().join("state/modules"),
        };
        let mut value = legacy();
        value["module_runtime"] = json!({"community.dandys-world":{"data_directory":path}});
        let config = load(root.path(), &value).unwrap();
        assert!(
            Host::open(&config, PgTools::default()).await.is_err(),
            "{case}"
        );
        assert!(!root.path().join("data").exists());
        let config = load(root.path(), &legacy()).unwrap();
        Host::open(&config, PgTools::default())
            .await
            .unwrap()
            .close()
            .await
            .unwrap();
    }
}
#[tokio::test]
async fn valid_dedicated_runtime_directory_is_prepared_without_discord_or_ai() {
    let root = scratch();
    let directory = root.path().join("dw-data");
    let mut value = legacy();
    value["module_runtime"] = json!({"community.dandys-world":{"data_directory":directory,"citation_prefix":"https://example.org/index.php?oldid="}});
    let config = load(root.path(), &value).unwrap();
    let host = Host::open(&config, PgTools::default()).await.unwrap();
    let module = ModuleId::new("community.dandys-world").unwrap();
    assert_eq!(
        host.modules
            .runtime_settings(&module)
            .unwrap()
            .data_directory,
        Some(directory.clone())
    );
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
        0o700
    );
    host.close().await.unwrap();
}
