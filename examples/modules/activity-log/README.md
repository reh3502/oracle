# Activity log

This is a separately installed native module (`community.activity-log`). It records normalized Discord metadata using module-scoped document storage. It cannot read message bodies or attachments and has no raw Discord HTTP or SQL access.

`moderate/v1` enables moderation audit events, channel and role/access changes, member role changes, bans/unbans, and 15-minute join/leave summaries. It excludes messages, content, attachments, reactions, typing, presence, and routine voice movement. It requires `guilds`, `guild_members`, and `guild_moderation`; missing intent coverage must be reported by the host instead of silently calling a reduced configuration moderate.

The preset fixes metadata retention at 14 days, excludes Oracle-origin events, coalesces equivalent metadata for 30 seconds, and bounds the delivery queue at 128. Retained metadata uses 337 hourly slots with at most 128 records per hour. Overload drops new records and later reports a bounded aggregate. Stable accepted event IDs are deduplicated within their retained hourly records. Maintenance events remove expired records, flush membership summaries, and flush due notifications. Without maintenance, status reports the last completed retention run instead of claiming the job is healthy.

Configuration requires a destination channel ID; `operator_note` is preserved by preset merging. The host validates destination visibility, send permissions, and current intent availability. `status` reports the exact applied configuration and delivery/retention counters, and leaves host subscription and permission health explicitly unverified. `probe` requests one safe synthetic notification through the host's durable effect journal. A verified probe is tied to its destination and configuration revision; it does not imply that a real moderation event was observed.

Pending notifications are persisted before sending. Uncertain delivery retains the same purpose so the host journal prevents blind resends. Retention applies to database metadata; it does not delete Discord messages already delivered.

## Install and configure

Enable the server members intent for the bot in Discord's developer settings. Configure the host's `discord.intents` as `["guilds", "guild_members", "guild_moderation"]`, then restart the host. Use a private staff text channel that the bot can view, send to, and read message history in. Ordinary member roles must not have access to that channel.

From the repository root, build and stage the separate executable:

```sh
python3 scripts/module-dev.py --stage-only --profile activity-log
```

To install it into a running host and grant its declared capabilities, use the explicit reload mode. Replace the config path and guild ID:

```sh
python3 scripts/module-dev.py --reload --profile activity-log \
  --config /absolute/path/oracle.json --guild 123 --trust-native
```

Create a configuration plan with the staff channel's ID:

```sh
target/debug/oracle --config /absolute/path/oracle.json module-config \
  --guild 123 --module community.activity-log plan \
  --preset moderate/v1 --values '{"destination":"456"}'
```

Review the returned values, then apply the returned plan ID with `module-config --guild 123 --module community.activity-log apply --plan PLAN_ID`. Use `module-config ... inspect` to compare stored and effective revisions.

Check the module with `module invoke --guild 123 --module community.activity-log --operation status --input '{}'`. The host's `status --guild 123` also reports available event intents and module event queues. Run operation `probe` to request a synthetic notification in the configured channel; this sends a message. Inspect the verified delivery receipt and channel ID. A probe confirms delivery only: a real moderation event and the module's corresponding counters confirm event coverage. Missing old member-role observations are coverage gaps, not invented role changes.

The host sends maintenance every minute while running. Check the last retention run and queue counters after at least one minute. Discord messages already sent remain in the channel after local metadata expires.
