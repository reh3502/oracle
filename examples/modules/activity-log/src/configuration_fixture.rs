//! Opt-in metadata-only fixtures; ordinary module behavior is unchanged.
use serde_json::Value;

pub fn configuration_variant(mut manifest: Value, typed: bool) -> Value {
    if typed {
        manifest["configuration"]["presets"] = serde_json::json!({});
    } else {
        manifest
            .as_object_mut()
            .expect("manifest object")
            .remove("configuration");
        // Event subscriptions and notification delivery require configuration.
        // Keep a valid inspection-only module, rather than an invalid package.
        manifest["subscriptions"] = serde_json::json!([]);
        manifest["capabilities"] = serde_json::json!(["storage.own"]);
        manifest["required_intents"] = serde_json::json!([]);
        manifest["operations"]
            .as_array_mut()
            .expect("operations")
            .retain(|operation| operation["name"] == "status");
        manifest["operations"][0]["capabilities"] = serde_json::json!(["storage.own"]);
        manifest["commands"]["routes"]
            .as_array_mut()
            .expect("routes")
            .retain(|route| route["name"] == "status");
    }
    manifest
}
