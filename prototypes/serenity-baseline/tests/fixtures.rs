use oracle_serenity_baseline::{AdapterHandler, decode_dispatch, normalize};
use serde_json::{Value, json};
use serenity::all::*;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

#[test]
fn next_dispatch_handles_partial_events_and_string_snowflakes() {
    let raw = json!({"op":0,"s":1,"t":"MESSAGE_DELETE","d":{
        "id":"18446744073709551614","channel_id":"202","guild_id":"101","future_field":true
    }})
    .to_string();
    let event = decode_dispatch(&raw).unwrap();
    assert_eq!(
        normalize(&event).unwrap(),
        json!({"kind":"message.deleted","message_id":"18446744073709551614","channel_id":"202","guild_id":"101"})
    );
    let update = decode_dispatch(
        &json!({"op":0,"s":2,"t":"MESSAGE_UPDATE","d":{"id":"303","channel_id":"202"}}).to_string(),
    );
    assert_eq!(normalize(&update.unwrap()).unwrap()["content_known"], false);
    let _: &dyn EventHandler = &AdapterHandler::default();
}

#[test]
fn next_unknown_and_malformed_dispatches_are_explicit() {
    for (name, data) in [
        ("NEW_FUTURE_EVENT", json!({"future":true})),
        (
            "MESSAGE_DELETE",
            json!({"id":"not-an-id","channel_id":"202"}),
        ),
    ] {
        let result = decode_dispatch(&json!({"op":0,"s":3,"t":name,"d":data}).to_string());
        assert!(normalize(&result.unwrap()).is_none());
    }
    assert!(decode_dispatch("{broken").is_err());
}

#[test]
fn channel_visibility_gaps_are_not_silently_accepted_as_complete_channels() {
    let normal: GuildChannel = serde_json::from_value(channel(202, "visible", 0)).unwrap();
    assert_eq!(normal.base.name.as_str(), "visible");
    // Selected next requires name/type. A missing-name obfuscated payload must be
    // surfaced as unsupported, not turned into a confidently empty channel list.
    assert!(
        serde_json::from_value::<GuildChannel>(json!({"id":"202","guild_id":"101","type":0}))
            .is_err()
    );
    let mut future = channel(202, "future", 254);
    future["new_discord_field"] = json!(true);
    let future: GuildChannel = serde_json::from_value(future).unwrap();
    assert_eq!(serde_json::to_value(future.base.kind).unwrap(), 254);
}

fn channel(id: u64, name: &str, kind: u8) -> Value {
    json!({"id":id.to_string(),"guild_id":"101","type":kind,"name":name,"position":0,"permission_overwrites":[]})
}
fn command() -> Value {
    json!({"id":"404","application_id":"505","guild_id":"101","type":1,
        "name":"oracle-p3-fixture","description":"P3 fixture","version":"606"})
}

async fn mock_http(
    responses: Vec<(u16, Value)>,
) -> (Http, tokio::task::JoinHandle<Vec<(String, Value)>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let mut captured = Vec::new();
        for (status, response) in responses {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let header_end = loop {
                let mut byte = [0; 1];
                stream.read_exact(&mut byte).await.unwrap();
                bytes.push(byte[0]);
                if bytes.ends_with(b"\r\n\r\n") {
                    break bytes.len();
                }
            };
            let headers = String::from_utf8(bytes.clone()).unwrap();
            let length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(str::trim)
                        .map(str::to_owned)
                })
                .map(|s| s.parse::<usize>().unwrap())
                .unwrap_or(0);
            bytes.resize(header_end + length, 0);
            stream.read_exact(&mut bytes[header_end..]).await.unwrap();
            let body = if length == 0 {
                Value::Null
            } else {
                serde_json::from_slice(&bytes[header_end..]).unwrap()
            };
            captured.push((headers.lines().next().unwrap().to_owned(), body));
            let response = if status == 204 {
                String::new()
            } else {
                response.to_string()
            };
            let wire = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                response.len()
            );
            stream.write_all(wire.as_bytes()).await.unwrap();
        }
        captured
    });
    // Fail closed even if the library accidentally bypasses its configured proxy.
    // No system proxy is inherited; Discord DNS is pinned to a closed loopback port.
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .resolve("discord.com", "127.0.0.1:9".parse().unwrap())
        .build()
        .unwrap();
    let http = HttpBuilder::new("fixture.only.token".parse().unwrap())
        .client(client)
        .proxy(proxy)
        .ratelimiter_disabled(true)
        .application_id(ApplicationId::new(505))
        .build();
    (http, task)
}

#[tokio::test]
async fn actual_http_channel_and_command_routes_builders_and_readback() {
    let (http, server) = mock_http(vec![
        (200, channel(202, "p3-category", 4)),
        (200, json!([channel(202, "p3-category", 4)])),
        (200, command()),
        (200, json!([command()])),
        (204, Value::Null),
        (200, channel(202, "p3-category", 4)),
    ])
    .await;
    let guild = GuildId::new(101);
    let category = http
        .create_channel(
            guild,
            &CreateChannel::new("p3-category").kind(ChannelType::Category),
            Some("P3 fixture"),
        )
        .await
        .unwrap();
    assert_eq!(category.id.get(), 202);
    assert_eq!(http.get_channels(guild).await.unwrap().len(), 1);
    let definition = CreateCommand::new("oracle-p3-fixture")
        .description("P3 fixture")
        .default_member_permissions(Permissions::MANAGE_GUILD);
    let created = http.create_guild_command(guild, &definition).await.unwrap();
    assert_eq!(created.name.as_str(), "oracle-p3-fixture");
    assert_eq!(
        http.get_guild_commands(guild).await.unwrap()[0].id,
        created.id
    );
    http.delete_guild_command(guild, created.id).await.unwrap();
    http.delete_channel(category.id.into(), Some("P3 fixture cleanup"))
        .await
        .unwrap();
    let requests = tokio::time::timeout(std::time::Duration::from_secs(3), server)
        .await
        .unwrap()
        .unwrap();
    let paths: Vec<_> = requests.iter().map(|r| r.0.as_str()).collect();
    assert_eq!(
        paths,
        vec![
            "POST /api/v10/guilds/101/channels HTTP/1.1",
            "GET /api/v10/guilds/101/channels HTTP/1.1",
            "POST /api/v10/applications/505/guilds/101/commands HTTP/1.1",
            "GET /api/v10/applications/505/guilds/101/commands HTTP/1.1",
            "DELETE /api/v10/applications/505/guilds/101/commands/404 HTTP/1.1",
            "DELETE /api/v10/channels/202 HTTP/1.1"
        ]
    );
    assert_eq!(requests[0].1["type"], 4);
    assert_eq!(
        requests[2].1["default_member_permissions"],
        Permissions::MANAGE_GUILD.bits().to_string()
    );
}

#[test]
fn actual_command_interaction_dispatch_and_ephemeral_ack_builder() {
    let raw = include_str!("../fixtures/interaction.json");
    let event = decode_dispatch(raw).unwrap();
    let payload: Value = serde_json::from_str(raw).unwrap();
    let tag_first = format!(
        r#"{{"t":"INTERACTION_CREATE","op":0,"s":4,"d":{}}}"#,
        payload["d"]
    );
    assert_eq!(
        normalize(&decode_dispatch(&tag_first).unwrap()),
        normalize(&event)
    );
    assert_eq!(
        normalize(&event).unwrap(),
        json!({"kind":"interaction.created","id":"707"})
    );
    assert!(
        !normalize(&event)
            .unwrap()
            .to_string()
            .contains("fake-interaction-token")
    );
    let defer =
        CreateInteractionResponse::Defer(CreateInteractionResponseMessage::new().ephemeral(true));
    let payload = serde_json::to_value(defer).unwrap();
    assert_eq!(payload["type"], 5);
    assert_eq!(payload["data"]["flags"], 64);
}

#[test]
fn partial_update_fallback_preserves_absent_empty_and_rejects_malformed() {
    for (content, known) in [(None, false), (Some(json!("")), true)] {
        let mut data = json!({"id":"303","channel_id":"202"});
        if let Some(content) = content {
            data["content"] = content;
        }
        let event =
            decode_dispatch(&json!({"op":0,"s":5,"t":"MESSAGE_UPDATE","d":data}).to_string())
                .unwrap();
        let normalized = normalize(&event).unwrap();
        assert_eq!(normalized["content_known"], known);
        assert_eq!(
            normalized["content"],
            if known { json!("") } else { Value::Null }
        );
    }
    let event=decode_dispatch(&json!({"op":0,"s":6,"t":"MESSAGE_UPDATE","d":{"id":"303","channel_id":"202","content":42}}).to_string()).unwrap();
    assert!(normalize(&event).is_none());
}
