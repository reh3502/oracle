//! Stage 0 P3 experiments against the exact Serenity next source revision.
use serde_json::{Value, json};
use serenity::all::{
    Cache, Context, EventHandler, FullEvent, GatewayEvent, GenericChannelId, MessageId,
};
use std::sync::Mutex;

pub const SERENITY_REV: &str = "98ec74223b0ff77fc4e8085d25569ea59e09a36f";

/// Real upstream Gateway deserialization and FullEvent construction, including cache updates.
pub fn decode_dispatch(raw: &str) -> Result<FullEvent, String> {
    match serde_json::from_str::<GatewayEvent>(raw).map_err(|e| e.to_string())? {
        GatewayEvent::Dispatch { event, .. } => Ok(FullEvent::from_event(
            event.into_event(),
            &mut None,
            &Cache::default(),
        )),
        _ => Err("not a supported dispatch".into()),
    }
}

/// No Serenity objects or interaction tokens escape this demonstration boundary.
pub fn normalize(event: &FullEvent) -> Option<Value> {
    match event {
        FullEvent::Unknown { event, .. } => normalize_partial_update(event.data.get()),
        FullEvent::MessageDelete {
            channel_id,
            deleted_message_id,
            guild_id,
            ..
        } => Some(json!({
            "kind": "message.deleted", "channel_id": channel_id.to_string(),
            "message_id": deleted_message_id.to_string(), "guild_id": guild_id.map(|id| id.to_string())
        })),
        FullEvent::MessageUpdate { event, .. } => Some(json!({
            "kind": "message.updated", "channel_id": event.message.channel_id.to_string(),
            "message_id": event.message.id.to_string(), "content_known": true, "content": event.message.content.as_str()
        })),
        FullEvent::InteractionCreate { interaction, .. } => Some(json!({
            "kind": "interaction.created", "id": interaction.id().to_string()
        })),
        _ => None,
    }
}

/// Compile the exact next EventHandler contract; fixtures exercise the same translation used by this dispatch hook.
#[derive(Default)]
pub struct AdapterHandler {
    pub events: Mutex<Vec<Value>>,
}
#[async_trait::async_trait]
impl EventHandler for AdapterHandler {
    async fn dispatch(&self, _context: &Context, event: &FullEvent) {
        if let Some(record) = normalize(event) {
            self.events
                .lock()
                .expect("event recorder lock")
                .push(record);
        }
    }
}

/// Recover only the explicitly understood partial-message shape. Missing content
/// remains unknown; an explicit empty string remains a known empty value.
fn normalize_partial_update(raw: &str) -> Option<Value> {
    let envelope: Value = serde_json::from_str(raw).ok()?;
    if envelope["t"].as_str()? != "MESSAGE_UPDATE" {
        return None;
    }
    let data = envelope.get("d")?.as_object()?;
    let message: MessageId = serde_json::from_value(data.get("id")?.clone()).ok()?;
    let channel: GenericChannelId = serde_json::from_value(data.get("channel_id")?.clone()).ok()?;
    let content = match data.get("content") {
        Some(value) => Some(value.as_str()?),
        None => None,
    };
    Some(
        json!({"kind":"message.updated", "message_id":message.to_string(),
        "channel_id":channel.to_string(), "content_known":content.is_some(), "content":content}),
    )
}
