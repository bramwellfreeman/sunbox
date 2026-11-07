# ☀️ Sunbox — your inbox, at sunrise

Gmail in the terminal for Linux: a keyboard-driven inbox for you, plus a CLI and MCP server so AI
agents can read, draft and send mail, and keep **contact profiles**, **templates** and **routines**.
Formerly "Crayon Box". Sibling of the Dioxus desktop app (`~/Projects/crayon-box` on the Mac); same Google OAuth client and the
same privacy posture: mail goes straight to the Gmail API and everything else stays local.

## Install

On a Mac (Apple Silicon or Intel) or Linux (x86_64), in Terminal:

```sh
curl -fsSL https://github.com/bramwellfreeman/sunbox/releases/latest/download/install.sh | sh
sunbox auth setup        # paste the "Crayon Box" OAuth client secret
sunbox auth login        # browser consent (unverified-app warning → Advanced → Continue)
sunbox                   # inbox
```

It installs to `~/.local/bin` (set `SUNBOX_BIN` to change that) and adds it to your PATH if needed.
Downloading with curl keeps macOS from quarantining the binary, so it runs without a Gatekeeper prompt.
From source: `cargo build --release && install -Dm755 target/release/sunbox ~/.local/bin/sunbox`.
Releases are built by `.github/workflows/release.yml` when a `v*` tag is pushed.

## Use it

- **TUI**: `sunbox`. Tabs To do · Primary · Starred · Newsletters · All · Sent (who you wrote to, and who hasn't replied yet) · ✦ Claude. `↵` opens a thread, `r` replies,
  `T` replies from a template, `c` composes, `f` forwards, `e` archives, `s` stars, `l` labels,
  `/` searches, `C` finds a contact, `?` shows help. In To/Cc/Bcc, start typing a name or address
  and pick a suggestion with ↑↓ + Tab/↵. You write in `$EDITOR`, then choose send / draft / edit again / discard.
- **CLI**: `sunbox inbox`, `search`, `read`, `send`, `archive`, `contacts find <name>`, `contacts …`, `templates …`, `routines …`.
  Add `--json` for machine output.
- **Agents**: `sunbox agent install claude` registers the MCP server. See [AGENTS.md](AGENTS.md).
- **Routines**: `sunbox routines install-timer` adds a systemd user timer that runs due routines.
- **Accounts**: run `sunbox auth login` again to add another Google account. `sunbox auth list` shows them,
  `sunbox auth default <email>` picks the default, and `--account <email>` (or `$SUNBOX_ACCOUNT`) works
  on any command. In the TUI, `a` switches, adds, sets the default or signs out.
- **Several inboxes at once**: in the TUI press `a`, tick inboxes with space (`A` ticks all) and press ↵.
  They merge into one list, newest first, with each row tagged by account. Replies and actions go
  through the right account, and Ctrl+F in the composer picks the sender for new mail.
- **⚙ Settings** (top right, or `,`): **Accounts** (switch, add, default, sign out, show inboxes
  together) and **AI assistant**: pick Claude (Claude Code or an API key), ChatGPT (Codex CLI or an
  OpenAI key) Gemini (Gemini CLI or a Google AI Studio key) or Grok (Grok CLI or an xAI key). ↵ signs in if needed and switches,
  `m` sets the model, `t` sends a test question, `u` tells it about you. Keys are saved in the
  config file (chmod 600); `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `GEMINI_API_KEY` / `XAI_API_KEY` work too.
- **✦ Claude** (tab 7): Claude reads the inbox, picks categories that fit it (Clients, Bills, School…),
  sorts every email under one with a priority and a one-line reason, and drafts a reply to each email
  that needs one. Emails that need something else get one-press buttons Claude labels itself:
  add to Google Calendar (opens prefilled for you to save), add a TickTick task (by email: set
  `[tasks] email` or enter it on first use), open the link the email is about, unsubscribe
  (one-click, email or page), archive, star, label or forward. `space` opens them, `F` files every
  email into a `Sunbox/<category>` Gmail label. ↵ opens a draft in the composer to check, edit and send. Nothing is sent
  without you, and sending is blocked while `[[gaps]]` Claude couldn't fill remain. `w` redrafts with
  your note, `L` has Claude study a sender. It runs on the Claude Code CLI (your plan) or, with
  `ANTHROPIC_API_KEY`, the API. Set `model`, `effort` and an `about` line under `[claude]` in the config.
  CLI: `sunbox claude brief|actions|do|draft|drafts|learn|template|status`.
- **Handled another way** (`H`, or the last button under space in ✦ Claude): for mail you dealt with
  by phone, in person or elsewhere. Add a short note and it's archived (labelled `Sunbox/Handled`),
  the note goes on the sender's profile, and later briefings treat similar mail as important without
  suggesting a follow-up. `sunbox handled <id> --note …`; `sunbox handled` lists them.
- **Ask Claude while writing** (Ctrl+K in the composer): say what the email should do ("tell her
  Saturday works, ask about parking") or how to change it ("shorter and warmer"). Claude writes or
  rewrites the body in place, using the thread, the recipients' profiles and your templates, and
  fills an empty subject. Ctrl+Z puts your version back; things to double-check show under the text.
- **Autocorrect** while you write: common typos are fixed the moment a word ends (teh → the,
  dont → don't, i → I), and a second after you pause Claude fixes spelling and capitals (never
  wording). Fixed words flash; Ctrl+Z undoes the last fix and won't redo it; Ctrl+G turns it off.
  `[claude] autocorrect = false` turns it off by default; `autocorrect_model` / `autocorrect_effort`
  (default `low`) tune speed.
- **Templates** (`t`) and **People** (`P`): edit everything in the app. `w` has Claude write a
  template from a description and `W` revises one, both opening for review. Client profiles grow a
  "learned by Claude" section from your mail, and you can edit it like any other field.

## Where things live

- `~/.config/sunbox/config.toml` (macOS: `~/Library/Application Support/sunbox/`): OAuth client and agent permissions (chmod 600)
- `~/.local/share/sunbox/sunbox.db` (macOS: same folder as the config): tokens, contacts, templates, routines, audit log (chmod 600)

## Code

`src/engine.rs` is the facade every front end shares, and it does the permission checks and audit
logging. `gmail.rs` is the REST client (parallel thread fetch), `oauth.rs` handles loopback + PKCE,
`cleaning.rs` strips quotes and signatures, `templates.rs` renders placeholders, `mcp.rs` is the MCP
server, `tui/` is the ratatui front end, and `store.rs` is SQLite. `cargo test` runs the unit tests.

TODO: move refresh tokens to the Secret Service keyring, attachment download, local cache +
`history.list` sync, HTML-rich reading.
