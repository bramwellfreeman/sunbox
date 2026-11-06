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
