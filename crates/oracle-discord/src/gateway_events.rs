//! Metadata-only Gateway normalization; raw event payloads never enter module RPC.
use super::*;
use oracle_core::{GuildEvent, GuildEventKind, GuildEventOrigin, OperationId};
use std::collections::BTreeSet;

pub(super) fn intents(names: &BTreeSet<String>) -> oracle_core::Result<discord::GatewayIntents> {
    let mut result = discord::GatewayIntents::empty();
    for name in names {
        result |= match name.as_str() {
            "guilds" => discord::GatewayIntents::GUILDS,
            "guild_members" => discord::GatewayIntents::GUILD_MEMBERS,
            "guild_moderation" => discord::GatewayIntents::GUILD_MODERATION,
            _ => return Err(oracle_core::Error::new(ErrorCode::InvalidInput)),
        };
    }
    if !names.contains("guilds") {
        return Err(oracle_core::Error::new(ErrorCode::InvalidInput));
    }
    Ok(result)
}
pub(super) enum Normalized {
    Event(GuildId, GuildEvent),
    MemberRolesGap(GuildId),
    Ignored,
}
pub(super) fn normalize(event: &discord::FullEvent, bot: u64, received_at_ms: u64) -> Normalized {
    use GuildEventKind as K;
    use discord::FullEvent as F;
    match event {
        F::ChannelUpdate {
            old: Some(old),
            new,
            ..
        } if old.base.guild_id != new.base.guild_id || old.id != new.id => {
            return Normalized::Ignored;
        }
        F::GuildRoleUpdate {
            old_data_if_available: Some(old),
            new,
            ..
        } if old.guild_id != new.guild_id || old.id != new.id => return Normalized::Ignored,
        _ => {}
    }
    let (guild, kind, subject, actor, related, stable, origin, at) = match event {
        F::GuildAuditLogEntryCreate {
            entry, guild_id, ..
        } => (
            *guild_id,
            K::ModerationAudit,
            entry.target_id.map(|id| id.to_string()),
            entry.user_id.map(|id| id.to_string()),
            None,
            Some(format!("audit:{}", entry.id)),
            if entry.user_id.is_some_and(|id| id.get() == bot) {
                GuildEventOrigin::Oracle
            } else {
                GuildEventOrigin::External
            },
            u64::try_from(entry.id.created_at().unix_timestamp_millis()).unwrap_or(received_at_ms),
        ),
        F::ChannelCreate { channel, .. }
        | F::ChannelUpdate { new: channel, .. }
        | F::ChannelDelete { channel, .. }
        | F::CategoryCreate {
            category: channel, ..
        }
        | F::CategoryDelete {
            category: channel, ..
        } => (
            channel.base.guild_id,
            K::ChannelChanged,
            Some(channel.id.to_string()),
            None,
            channel.parent_id.map(|id| id.to_string()),
            None,
            GuildEventOrigin::External,
            received_at_ms,
        ),
        F::GuildRoleCreate { new: role, .. } | F::GuildRoleUpdate { new: role, .. } => (
            role.guild_id,
            K::RoleAccessChanged,
            Some(role.id.to_string()),
            None,
            None,
            None,
            GuildEventOrigin::External,
            received_at_ms,
        ),
        F::GuildRoleDelete {
            guild_id,
            removed_role_id,
            ..
        } => (
            *guild_id,
            K::RoleAccessChanged,
            Some(removed_role_id.to_string()),
            None,
            None,
            None,
            GuildEventOrigin::External,
            received_at_ms,
        ),
        F::GuildBanAddition {
            guild_id,
            banned_user,
            ..
        } => (
            *guild_id,
            K::Ban,
            Some(banned_user.id.to_string()),
            None,
            None,
            None,
            GuildEventOrigin::External,
            received_at_ms,
        ),
        F::GuildBanRemoval {
            guild_id,
            unbanned_user,
            ..
        } => (
            *guild_id,
            K::Unban,
            Some(unbanned_user.id.to_string()),
            None,
            None,
            None,
            GuildEventOrigin::External,
            received_at_ms,
        ),
        F::GuildMemberAddition { new_member, .. } => (
            new_member.guild_id,
            K::MemberJoined,
            Some(new_member.user.id.to_string()),
            None,
            None,
            None,
            GuildEventOrigin::External,
            received_at_ms,
        ),
        F::GuildMemberRemoval { guild_id, user, .. } => (
            *guild_id,
            K::MemberLeft,
            Some(user.id.to_string()),
            None,
            None,
            None,
            GuildEventOrigin::External,
            received_at_ms,
        ),
        F::GuildMemberUpdate {
            old_if_available,
            event,
            new,
            ..
        } => {
            let Ok(guild) = GuildId::new(event.guild_id.to_string()) else {
                return Normalized::Ignored;
            };
            let Some(old) = old_if_available else {
                return Normalized::MemberRolesGap(guild);
            };
            if old.guild_id != event.guild_id
                || old.user.id != event.user.id
                || new.as_ref().is_some_and(|new| {
                    new.guild_id != event.guild_id || new.user.id != event.user.id
                })
            {
                return Normalized::MemberRolesGap(guild);
            }
            let before: BTreeSet<_> = old.roles.iter().copied().collect();
            let after: BTreeSet<_> = event.roles.iter().copied().collect();
            if before == after {
                return Normalized::Ignored;
            }
            (
                event.guild_id,
                K::MemberRolesChanged,
                Some(event.user.id.to_string()),
                None,
                None,
                None,
                GuildEventOrigin::External,
                received_at_ms,
            )
        }
        _ => return Normalized::Ignored,
    };
    let Ok(guild) = GuildId::new(guild.to_string()) else {
        return Normalized::Ignored;
    };
    // Audit IDs survive replay. Other FullEvent variants expose no Gateway sequence;
    // assign one host envelope ID and preserve it through all queue/retry operations.
    Normalized::Event(
        guild,
        GuildEvent {
            id: stable.unwrap_or_else(|| format!("gateway:{}", OperationId::generate())),
            kind,
            occurred_at_ms: at.max(1),
            origin,
            subject_id: subject,
            actor_id: actor,
            related_id: related,
        },
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    fn event(name: &str, data: Value) -> discord::FullEvent {
        let raw = json!({"op":0,"s":12,"t":name,"d":data});
        let gateway: discord::GatewayEvent = serde_json::from_str(&raw.to_string()).unwrap();
        let discord::GatewayEvent::Dispatch { event, .. } = gateway else {
            panic!("notdispatch");
        };
        discord::FullEvent::from_event(event.into_event(), &mut None, &discord::Cache::default())
    }
    #[test]
    fn audit_metadata_stable_identity_and_self_exclusion() {
        let input = event(
            "GUILD_AUDIT_LOG_ENTRY_CREATE",
            json!({"guild_id":"101","id":"1000000000000000000","action_type":22,"target_id":"303","user_id":"505","reason":"PRIVATE_REASON","changes":[]}),
        );
        let Normalized::Event(guild, one) = normalize(&input, 505, 1000) else {
            panic!("missingevent");
        };
        let Normalized::Event(_, two) = normalize(&input, 505, 2000) else {
            panic!("missingevent");
        };
        assert_eq!(guild.as_str(), "101");
        assert_eq!(one.id, two.id);
        assert_eq!(one.origin, GuildEventOrigin::Oracle);
        assert_eq!(one.kind, GuildEventKind::ModerationAudit);
        let text = serde_json::to_string(&one).unwrap();
        assert!(!text.contains("PRIVATE_REASON"));
        assert!(!text.contains("changes"));
    }
    #[test]
    fn channel_metadata_excludes_names_topics_and_guild_is_exact() {
        let input = event(
            "CHANNEL_CREATE",
            json!({"guild_id":"101","id":"202","type":0,"name":"PRIVATE_NAME","topic":"PRIVATE_TOPIC","parent_id":"303","permission_overwrites":[]}),
        );
        let Normalized::Event(guild, output) = normalize(&input, 505, 1000) else {
            panic!("missingevent");
        };
        assert_eq!(guild.as_str(), "101");
        assert_ne!(guild.as_str(), "999");
        assert_eq!(output.subject_id.as_deref(), Some("202"));
        assert_eq!(output.related_id.as_deref(), Some("303"));
        let text = serde_json::to_string(&output).unwrap();
        assert!(!text.contains("PRIVATE_NAME"));
        assert!(!text.contains("PRIVATE_TOPIC"));
    }
    #[test]
    fn unavailable_old_member_is_reported_not_invented_role_change() {
        let input = event(
            "GUILD_MEMBER_UPDATE",
            json!({"guild_id":"101","user":{"id":"303","username":"private","discriminator":"0","avatar":null},"roles":["404"],"nick":"PRIVATE_NICK","joined_at":null}),
        );
        assert!(
            matches!(normalize(&input,505,1000),Normalized::MemberRolesGap(g) if g.as_str()=="101")
        );
    }
    #[test]
    fn member_roles_require_same_identity_and_actual_set_change() {
        let mut input = event(
            "GUILD_MEMBER_UPDATE",
            json!({"guild_id":"101","user":{"id":"303","username":"private","discriminator":"0","avatar":null},"roles":["404"],"joined_at":null}),
        );
        let member:discord::Member=serde_json::from_str(&json!({"guild_id":"101","user":{"id":"303","username":"private","discriminator":"0","avatar":null},"roles":["404"],"joined_at":null,"deaf":false,"mute":false,"flags":0}).to_string()).unwrap();
        if let discord::FullEvent::GuildMemberUpdate {
            old_if_available, ..
        } = &mut input
        {
            *old_if_available = Some(member);
        }
        assert!(matches!(normalize(&input, 505, 1000), Normalized::Ignored));
        if let discord::FullEvent::GuildMemberUpdate {
            old_if_available, ..
        } = &mut input
        {
            old_if_available.as_mut().unwrap().roles = Default::default();
        }
        assert!(
            matches!(normalize(&input,505,1000),Normalized::Event(_,e) if e.kind==GuildEventKind::MemberRolesChanged)
        );
        if let discord::FullEvent::GuildMemberUpdate {
            old_if_available, ..
        } = &mut input
        {
            old_if_available.as_mut().unwrap().guild_id = discord::GuildId::new(999);
        }
        assert!(matches!(
            normalize(&input, 505, 1000),
            Normalized::MemberRolesGap(_)
        ));
    }
    #[test]
    fn only_requested_intents_are_enabled() {
        let names = [
            "guilds".to_owned(),
            "guild_members".to_owned(),
            "guild_moderation".to_owned(),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            intents(&names).unwrap(),
            discord::GatewayIntents::GUILDS
                | discord::GatewayIntents::GUILD_MEMBERS
                | discord::GatewayIntents::GUILD_MODERATION
        );
        assert!(
            intents(
                &["guilds".into(), "message_content".into()]
                    .into_iter()
                    .collect()
            )
            .is_err()
        );
        assert!(intents(&BTreeSet::new()).is_err());
    }
    #[test]
    fn typing_event_never_reaches_modules() {
        let input = event(
            "TYPING_START",
            json!({"guild_id":"101","channel_id":"202","user_id":"303","timestamp":1000}),
        );
        assert!(matches!(normalize(&input, 505, 1000), Normalized::Ignored));
    }
}
