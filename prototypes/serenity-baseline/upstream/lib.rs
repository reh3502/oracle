//! Compile the exact unmodified upstream package and required feature combination.
pub fn required_intents() -> serenity::all::GatewayIntents {
    serenity::all::GatewayIntents::GUILDS
}

#[cfg(test)]
mod baseline_diagnostics {
    use serenity::all::{DeserializedEvent, GatewayEvent};
    #[test]
    fn unmodified_baseline_reproduces_partial_update_and_key_order_gaps() {
        for raw in [
            r#"{"op":0,"s":1,"t":"MESSAGE_UPDATE","d":{"id":"303","channel_id":"202"}}"#,
            include_str!("../fixtures/interaction.json"),
        ] {
            let event: GatewayEvent = serde_json::from_str(raw).unwrap();
            assert!(matches!(
                event,
                GatewayEvent::Dispatch {
                    event: DeserializedEvent::Unknown(_),
                    ..
                }
            ));
        }
    }
}
