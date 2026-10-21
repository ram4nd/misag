# raterm

A minimal Linux terminal: Tauri 2, a Rust PTY backend (`portable-pty`) and xterm.js, with Chrome-style tabs.

## Prerequisites

- Rust (stable) and Node.js with pnpm
- Tauri's Linux system libraries: `webkit2gtk-4.1`, `gtk3`, `libayatana-appindicator` (or `libappindicator-gtk3`), `librsvg`. See https://tauri.app/start/prerequisites/

## Commands

```sh
pnpm install
pnpm app:dev        # run with hot reload, as "raterm-dev" with its own data (see below)
pnpm tauri build    # release build, produces .deb and AppImage bundles
pnpm test           # frontend tests (Vitest, jsdom, mocked Tauri IPC)
cargo test --manifest-path src-tauri/Cargo.toml   # backend: sessions, database, hook, exec (runs real processes)
# Opt-in: drives a real Claude (checks /clear and resume; the resume test sends one short prompt).
# Needs a folder Claude already trusts.
RATERM_LIVE_DIR=~/some/trusted/project cargo test --manifest-path src-tauri/Cargo.toml --test claude_live -- --ignored
```

## Usage

| Action | How |
| --- | --- |
| New Claude tab (in the active tab's directory, or `$HOME`) | Click the Claude icon at the top right |
| New Claude tab with a name and directory | Shift+click the Claude icon |
| Rename a tab | Double-click it |
| Close a tab | Click ✕ or middle-click the tab |
| Restart a tab whose process exited or failed to start | Press Enter in it |
| Open a link | Click a URL in the terminal (opens in the system browser) |
| Toggle the sidebar | Burger button at the left of the tab bar |
| Settings | Click Settings at the bottom of the sidebar; it opens a Settings tab |

## Behaviour

- Tabs have a kind. Claude tabs run Claude Code. Terminal tabs (a login shell `$SHELL -l`, or `$SHELL -lc '<command>'`) are supported by the backend; the UI has no button for them. A leading `~` in the directory is expanded.
- Only one raterm runs at a time: launching it again focuses the open window. `pnpm app:dev` builds use the identifier `com.tag1.raterm.dev`, so a dev build runs next to an installed raterm with its own data directory instead of being refused. Debug builds also keep their data in a `dev/` subdirectory, so even a plain `pnpm tauri dev` never touches an installed raterm's data.
- Closing a tab sends SIGHUP to its process and, if it is still alive after 2 seconds, SIGKILL to its process group and to the foreground job's. Quitting raterm does the same for every tab, waiting at most those 2 seconds.
- When a process exits, its tab is struck through and shows the exit code. Pressing Enter in it starts it again in the same tab; a Claude tab comes back in the session it was last in.
- Each Claude tab has its own Claude Code session. A new tab starts `claude --session-id <uuid>`. On restore it runs `claude --resume <uuid>` if Claude saved a transcript for that session, and otherwise starts again under the same id. raterm passes Claude a `SessionStart` hook through `--settings` (your own Claude config is untouched), so whenever a session starts or changes inside a tab (`/clear`, `/resume`), Claude runs `raterm --claude-hook`, which records the tab's current session for the next restore.
- Tabs inherit raterm's environment, minus the variables a Claude Code session sets to mark itself (`CLAUDECODE`, `CLAUDE_CODE_SESSION_ID`, its messaging socket, ...). If raterm itself was started from Claude Code, the settings Claude Code gives its commands (`GIT_EDITOR=true`, `DISABLE_UPDATES`, `CLAUDE_EFFORT`, ...) are dropped too. Your own settings such as `CLAUDE_CONFIG_DIR`, `CLAUDE_CODE_USE_BEDROCK` and `ANTHROPIC_*` are kept.
- The Settings tab has one setting: whether Claude tabs start with `--dangerously-skip-permissions` (added before the session flags above). A change is saved right away and applies to Claude tabs started, restarted or restored afterwards; each Claude tab started that way shows a red **!**. The page reads the stored value every time it opens, so it shows what new tabs will use. The Settings tab is not saved with the other tabs. If the stored settings cannot be read, Claude tabs start with the defaults (Claude asks before running tools) and the page shows the error and cannot change them. Without a database (see Troubleshooting) settings last only until raterm quits.
- Claude is started through your login shell so it sees your usual `PATH`. Under fish, csh and other non-POSIX shells raterm runs itself as `raterm --exec` from the login shell, which then starts `claude`.
- App data lives in SQLite at `$XDG_DATA_HOME/com.tag1.raterm/raterm.db` (by default `~/.local/share/com.tag1.raterm/`, WAL mode): the open tabs with each tab's directory as of the last save, the settings (one row in the `settings` table, a column per setting), app state, and each Claude tab's current session. Tabs are saved shortly after every change and on window close, and reopened on the next launch. A saved directory that no longer exists falls back to `$HOME`. With nothing saved (or after closing every tab) the app starts with no tabs.
- If saving is not possible, a notice above the terminals says so. A failed save is retried every few seconds. If saving fails when the window closes, the close is cancelled once with a warning; closing again quits without saving. raterm never saves over tabs it could not read, including tabs a newer raterm wrote.
- If a Claude tab's session hook has not reported within a minute of the tab starting, a notice says that `/clear` and `/resume` in it will not be remembered. It clears once the hook reports.
- If the session hook logs a failure, a notice shows it with the path of the hook log. It clears once the hook reports again for the tab it failed for.
- A dismissed notice stays hidden until something new happens.

## Troubleshooting

- **A notice says tabs are not being saved.** The database could not be opened (permissions, a newer raterm's schema, no data directory). raterm runs on an empty in-memory database and leaves the file alone; Claude sessions are not tracked in that mode.
- **`raterm.db.broken-<time>` files.** A damaged database was moved aside and replaced; the old file is kept there for recovery and can be deleted.
- **Claude tabs do not start.** `claude` must be on your login shell's `PATH`.
- **Claude tabs do not come back in the right conversation.** Check `hook.log` next to `raterm.db` (the hook-failure notice shows its path), where the session hook records its failures. Hooks must be allowed in your Claude settings, and in a folder Claude does not trust yet the hook only starts reporting after you accept Claude's trust prompt. If you use `CLAUDE_CONFIG_DIR`, set it where raterm itself sees it, not only in a shell profile.
- **After upgrading raterm**, restart it. A new version may change the database schema, and the session hook (which runs the new binary) refuses a schema it does not know until the app has been restarted and migrated it.
- **Diagnostics.** The backend prints to stderr with a `raterm:` prefix; start `raterm` from a terminal to see them.
