# ☀️ Sunbox — your inbox, at sunrise

Gmail in the terminal for Linux: a keyboard-driven inbox for you, plus a CLI and MCP server so AI
agents can read, draft and send mail, and keep **contact profiles**, **templates** and **routines**.
Formerly "Crayon Box". Sibling of the Dioxus desktop app (`~/Projects/crayon-box` on the Mac); same Google OAuth client and the
same privacy posture: mail goes straight to the Gmail API and everything else stays local.

## Install

```sh
cargo build --release && install -Dm755 target/release/sunbox ~/.local/bin/sunbox
sunbox auth setup        # paste the "Crayon Box" OAuth client secret
sunbox auth login        # browser consent (unverified-app warning → Advanced → Continue)
sunbox                   # inbox
```

## Use it

- **TUI**: `sunbox`. Tabs To do · Primary · Starred · Newsletters · All. `↵` opens a thread, `r` replies,
  `T` replies from a template, `c` composes, `f` forwards, `e` archives, `s` stars, `l` labels,
  `/` searches, `?` shows help. You write in `$EDITOR`, then choose send / draft / edit again / discard.
- **CLI**: `sunbox inbox`, `search`, `read`, `send`, `archive`, `contacts …`, `templates …`, `routines …`.
  Add `--json` for machine output.
- **Agents**: `sunbox agent install claude` registers the MCP server. See [AGENTS.md](AGENTS.md).
- **Routines**: `sunbox routines install-timer` adds a systemd user timer that runs due routines.
- **Accounts**: run `sunbox auth login` again to add another Google account. `sunbox auth list` shows them,
  `sunbox auth default <email>` picks the default, and `--account <email>` (or `$SUNBOX_ACCOUNT`) works
  on any command. In the TUI, `a` switches, adds, sets the default or signs out.
- **Several inboxes at once**: in the TUI press `a`, tick inboxes with space (`A` ticks all) and press ↵.
  They merge into one list, newest first, with each row tagged by account. Replies and actions go
  through the right account, and Ctrl+F in the composer picks the sender for new mail.
- **✦ Claude** (tab 6): Claude reads the inbox, ranks what matters with a one-line reason, and drafts
  replies to the urgent ones. ↵ opens a draft in the composer to check, edit and send. Nothing is sent
  without you, and sending is blocked while `[[gaps]]` Claude couldn't fill remain. `w` redrafts with
  your note, `L` has Claude study a sender. It runs on the Claude Code CLI (your plan) or, with
  `ANTHROPIC_API_KEY`, the API. Set `model`, `effort` and an `about` line under `[claude]` in the config.
  CLI: `sunbox claude brief|draft|drafts|learn|template|status`.
- **Templates** (`t`) and **People** (`P`): edit everything in the app. `w` has Claude write a
  template from a description and `W` revises one, both opening for review. Client profiles grow a
  "learned by Claude" section from your mail, and you can edit it like any other field.

## Where things live

- `~/.config/sunbox/config.toml`: OAuth client and agent permissions (chmod 600)
- `~/.local/share/sunbox/sunbox.db`: tokens, contacts, templates, routines, audit log (chmod 600)

## Code

`src/engine.rs` is the facade every front end shares, and it does the permission checks and audit
logging. `gmail.rs` is the REST client (parallel thread fetch), `oauth.rs` handles loopback + PKCE,
`cleaning.rs` strips quotes and signatures, `templates.rs` renders placeholders, `mcp.rs` is the MCP
server, `tui/` is the ratatui front end, and `store.rs` is SQLite. `cargo test` runs the unit tests.

TODO: move refresh tokens to the Secret Service keyring, attachment download, local cache +
`history.list` sync, HTML-rich reading.
