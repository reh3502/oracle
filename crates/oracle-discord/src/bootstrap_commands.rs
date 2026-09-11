//! Exact supported bootstrap definitions for the single durable command reconciler.
use oracle_core::{Error, ErrorCode, ModuleId, Result};
use oracle_operations::commands::{DesiredCommand, canonical_definition};
use serde_json::Value;

pub fn current_definition() -> Result<Value> {
    canonical_definition(
        &serde_json::to_value(crate::oracle_command())
            .map_err(|_| Error::new(ErrorCode::Integrity))?,
    )
}
/// Stage 1/2 exposed only status and scoped pause/resume. Those two descriptors
/// match commit 0c825d3, before operation groups were added by 685df37.
pub fn legacy_definition() -> Result<Value> {
    let mut definition = current_definition()?;
    let options = definition
        .get_mut("options")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| Error::new(ErrorCode::Integrity))?;
    if options.len() < 2 || options[0]["name"] != "status" || options[1]["name"] != "control" {
        return Err(Error::new(ErrorCode::Integrity));
    }
    options.truncate(2);
    Ok(definition)
}
pub fn known_definitions() -> Result<Vec<Value>> {
    let mut stage3 = current_definition()?;
    stage3["options"]
        .as_array_mut()
        .ok_or_else(|| Error::new(ErrorCode::Integrity))?
        .retain(|option| option["name"] != "agent");
    Ok(vec![current_definition()?, stage3, legacy_definition()?])
}
pub fn desired() -> Result<DesiredCommand> {
    Ok(DesiredCommand {
        owner: ModuleId::new("oracle.bootstrap")?,
        definition: current_definition()?,
        route: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn known_legacy_definition_matches_original_bootstrap_contract() {
        let expected = json!({"name":"oracle","description":"Oracle framework administration","type":1,"default_member_permissions":"32","options":[{"name":"status","description":"Show framework status","type":1},{"name":"control","description":"Pause or resume this server","type":1,"options":[{"name":"action","description":"Requested control","type":3,"required":true,"choices":[{"name":"pause","value":"pause"},{"name":"resume","value":"resume"}]}]}]});
        assert_eq!(
            legacy_definition().unwrap(),
            canonical_definition(&expected).unwrap()
        );
        let current = desired().unwrap();
        assert_eq!(current.owner.as_str(), "oracle.bootstrap");
        assert!(current.route.is_none());
        assert_eq!(current.definition["options"].as_array().unwrap().len(), 5);
    }
    #[test]
    fn known_definitions_match_real_discord_command_readback() {
        for definition in known_definitions().unwrap() {
            let mut value = definition.clone();
            value["id"] = json!("404");
            value["application_id"] = json!("505");
            value["guild_id"] = json!("101");
            value["version"] = json!("606");
            let command: serenity::all::Command = serde_json::from_value(value).unwrap();
            assert_eq!(
                canonical_definition(&serde_json::to_value(command).unwrap()).unwrap(),
                definition
            );
        }
    }
}
