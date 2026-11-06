# Sunbox — agent guide

Sunbox is the user's Gmail plus a local store of **contact profiles**, **templates** and
**routines**. Two ways in, same engine, same permissions:

- **MCP** (preferred): `sunbox mcp` on stdio. Register with `sunbox agent install claude`
  (or `codex`), or point any MCP client at `sunbox mcp`.
- **CLI**: every command takes `--json`. `sunbox --help`, `sunbox <cmd> --help`.

## Accounts

Several Google accounts can be signed in. Every MCP tool takes an optional `account` (an email);
without it you act as the default account. `whoami` lists `accounts` and `default_account`.
CLI: `--account <email>` on any command. Contacts and templates are shared across accounts; a
routine can set `"account"` to watch a specific one.

## Permissions

The user decides what agents may do (`sunbox agent permissions`):

| permission | allows | default |
|---|---|---|
| read | list, search, read mail | ✓ |
| draft | create Gmail drafts | ✓ |
| send | send without a human in the loop | ✗ |
| modify | archive, star, label, mark read | ✓ |
| manage | create/edit contacts, templates, routines | ✓ |
| exec | routines may run shell commands | ✗ |

Denied calls fail with a message that tells the user how to grant the permission. Every agent and routine
action goes into the audit log (`sunbox agent audit` / the `get_audit_log` tool). **Prefer drafts**
unless the user explicitly asked you to send.

## Mail

| MCP tool | CLI |
|---|---|
| `inbox_triage` | `sunbox inbox --filter todo\|primary\|bulk\|starred --json` |
| `search_threads {query}` | `sunbox search 'from:x newer_than:7d' --json` |
| `get_thread {thread_id, format}` | `sunbox read <id> [--full] --json` |
| `preview_email` / `create_draft` / `send_email` | `sunbox send --to … --body … [--reply <id>] [--template t --var k=v] [--draft\|--dry-run] -y` |
| `archive_thread`, `star_thread`, `mark_read`, `label_thread` | `sunbox archive\|star\|mark-read\|label` |

`get_thread` returns a `transcript`: quoted history and signatures stripped, one block per message.
That's the cheapest way to read a conversation. Replies need only `reply_to_thread`: the recipient,
`Re:` subject and In-Reply-To headers fill themselves in.

## Contact profiles

```json
{ "email": "jane@hall.org", "name": "Jane Doe", "company": "St. Mark Hall", "role": "Events",
  "relationship": "venue", "tone": "warm, brief, no emoji", "tags": ["vip"],
  "notes": "Prefers calls before 10am", "fields": { "instrument": "cello" } }
```

`upsert_contact` merges: only the fields you send change, tags are added, and `fields` keys merge
(set a key to `""` to delete it). Check `get_contact` before writing to someone and follow their `tone`.
`suggest_contacts` lists frequent correspondents with no profile yet. `learned` is a running
summary that Sunbox's built-in Claude assistant keeps up to date (the user can edit it too); read it
before writing to someone.
CLI: `sunbox contacts set jane@hall.org --tone "warm" --field instrument=cello` or `--from-json -`.

## Templates

`{ "name": "booking-ack", "subject": "Recording on {{date_wanted}}", "body": "Hi {{first_name}}, …" }`

Placeholders: `{{name}} {{first_name}} {{email}} {{my_email}} {{date}} {{subject}}`, any profile field
as `{{contact.company}}` / `{{contact.<custom>}}`, plus any `vars` you pass. Unfilled placeholders are
reported (`missing_vars`), and `send` refuses to go out with any left. Use `render_template` to preview.

## Routines

A routine selects threads with a Gmail query and runs ordered actions on each **new** match (a thread
re-triggers only when a newer message arrives). `sunbox routines tick` runs the due ones; the user's
systemd timer (`sunbox routines install-timer`) calls it every few minutes.

```json
{
  "name": "venue-inquiries",
  "description": "Label venue mail, draft an acknowledgement, ping me",
  "query": "in:inbox from:@hall.org is:unread",
  "every_minutes": 10,
  "enabled": true,
  "actions": [
    { "type": "label", "name": "Venues" },
    { "type": "draft", "template": "booking-ack", "vars": { "date_wanted": "TBD" } },
    { "type": "notify", "message": "Venue mail from {{from}}: {{subject}}" }
  ]
}
```

Actions: `label {name}`, `archive`, `star`, `mark_read`, `draft {template, vars}`,
`send {template, vars}` (needs `send`), `notify {message}`, and `exec {command}` (needs `exec`).
`exec` gets the thread summary as JSON on stdin plus `SUNBOX_THREAD_ID`, `SUNBOX_FROM` and
`SUNBOX_SUBJECT`, so it can hand off to another agent, e.g.
`claude -p "Summarise thread $SUNBOX_THREAD_ID with the sunbox tools and save a draft reply"`.

Test with `run_routines {name, dry_run: true}` (CLI: `sunbox routines run <name> --dry-run`) before
relying on it. CLI create: `sunbox routines set routine.json` (or `-` for stdin).
