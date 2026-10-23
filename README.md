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
| New terminal tab (in the active tab's directory, or `$HOME`) | Click the terminal icon at the top right |
| New terminal or Claude tab with a name and directory | Shift+click the terminal icon |
| New Claude tab in a folder | Hover the folder (or the workspace folder at the top) in the Workspace box and click its Claude icon |
| Rename a tab | Double-click it |
| Close a tab | Click ✕ or middle-click the tab |
| Restart a tab whose process exited or failed to start | Press Enter in it |
| Open a link | Click a URL in the terminal (opens in the system browser) |
| Toggle the sidebar | Burger button at the left of the tab bar |
| Browse your workspace | Workspace box in the sidebar: click a folder to open it; the reload button refreshes it; the gear opens the Workspace settings tab |
| Change the workspace folder | On the Workspace settings tab, click the path to pick a folder; the house button goes back to your home directory |
| Type a file's path into the active tab | Click the file in the Workspace box (the tab must be running) |
| Settings | Click Settings at the bottom of the sidebar; it opens a Settings tab |

## Behaviour

- Tabs have a kind. Claude tabs run Claude Code. Terminal tabs run a login shell (`$SHELL -l`, or `$SHELL -lc '<command>'` for a saved command). A leading `~` in the directory is expanded.
- A tab is labelled with the folder its process is in (`~` for your home directory), checked every 2 seconds so it follows `cd`; a tab you renamed keeps its name. The tooltip shows the terminal's title and the full directory. The icon is Claude's for a Claude tab, and for a terminal tab while Claude runs in it, and a terminal otherwise.
- A `claude` you start at a terminal tab's shell prompt is remembered too. Terminal tabs get `RATERM_TAB`, `RATERM_DB`, `RATERM_WRAPPER` and a `PATH` that starts with `<data dir>/bin`, where raterm writes a `claude` wrapper at every start (changes to it are lost). The wrapper runs `raterm --claude-launch`, which starts the real `claude` like a Claude tab: with the current settings (see the Settings tab below) and the same session hook, which also records Claude's pid; hook failures go to the same `hook.log`. Your own arguments come last. Without a usable database it starts Claude cautiously and untracked; if the raterm binary the wrapper points at has gone, the wrapper says so and runs `claude` unchanged. Every 2 seconds, and when the window closes, raterm checks whether each terminal tab's foreground program is the Claude that reported, and saves the tab with that session. On restore, a tab that runs a plain shell (not a saved command) gets `claude --resume <session>` typed at its prompt if Claude saved a conversation for it, so quitting Claude leaves you in the shell; otherwise a notice says why it was not reopened. The command is typed as soon as the shell starts, so a profile that reads input first (a passphrase prompt, an update question) or replaces the shell (`exec tmux`) takes it instead. A `claude` alias or shell function, or a profile that puts another directory holding `claude` before the wrapper on `PATH`, bypasses the wrapper; that session is not remembered, and a notice says so once it has run for a few seconds. If the wrapper cannot be written, a notice says so and terminal tabs run without it.
- Only one raterm runs at a time: launching it again focuses the open window. `pnpm app:dev` builds use the identifier `com.tag1.raterm.dev`, so a dev build runs next to an installed raterm with its own data directory instead of being refused. Debug builds also keep their data in a `dev/` subdirectory, so even a plain `pnpm tauri dev` never touches an installed raterm's data.
- Closing a tab sends SIGHUP to its process and, if it is still alive after 2 seconds, SIGKILL to its process group and to the foreground job's. Quitting raterm does the same for every tab, waiting at most those 2 seconds.
- When a process exits, its tab is struck through and shows the exit code. Pressing Enter in it starts it again in the same tab; a Claude tab comes back in the session it was last in.
- Each Claude tab has its own Claude Code session. A new tab starts `claude --session-id <uuid>`. On restore it runs `claude --resume <uuid>` if Claude saved a transcript for that session, and otherwise starts again under the same id. raterm passes Claude a `SessionStart` hook through `--settings` (your own Claude config is untouched), so whenever a session starts or changes inside a tab (`/clear`, `/resume`), Claude runs `raterm --claude-hook`, which records the tab's current session for the next restore.
- Tabs inherit raterm's environment, minus the variables a Claude Code session sets to mark itself (`CLAUDECODE`, `CLAUDE_CODE_SESSION_ID`, its messaging socket, ...) and the `RATERM_*` variables of a raterm tab it was started from (whose wrapper directory is also taken off `PATH`). If raterm itself was started from Claude Code, the settings Claude Code gives its commands (`GIT_EDITOR=true`, `DISABLE_UPDATES`, `CLAUDE_EFFORT`, ...) are dropped too. Your own settings such as `CLAUDE_CONFIG_DIR`, `CLAUDE_CODE_USE_BEDROCK` and `ANTHROPIC_*` are kept.
- The sidebar holds toolboxes; each one collapses from its header, and raterm remembers which are collapsed. The Workspace box shows your workspace folder: your home directory, or the folder you pick on the Workspace settings tab, which its gear button opens (for example your projects folder). It is stored with the settings. Folders are listed when opened (at most 2000 entries each, folders first, hidden files left out); the reload button refreshes the tree, for example after new files, and keeps open folders open. Clicking a file types its path, shell-quoted if needed and followed by a space, into the active tab and switches to it. A path whose name holds control characters or a backslash is not typed (they could act as keys, or confuse fish's quoting); a notice says so. A workspace folder that has gone since you chose it shows an error in the tree but never blocks saving other settings.
- The Settings tab has two settings. Skip permission prompts starts Claude tabs with `--dangerously-skip-permissions` (added before the session flags above). It also reaches every `claude` started in a terminal tab through the wrapper, including one a script or tool runs (`claude -p`). A tab whose Claude runs that way, or with `--permission-mode bypassPermissions`, says so in its tooltip; a `permissions.defaultMode` in your own Claude settings is not detected. Auto memory set to Disable adds `"autoMemoryEnabled": false` to the `--settings` JSON raterm passes Claude (next to the session hook, when there is one), in Claude tabs and terminal tabs alike; Auto (the default) leaves the key out, so your own Claude config decides. `CLAUDE_CODE_DISABLE_AUTO_MEMORY=0` in raterm's environment overrides it. A change is saved right away and applies to Claude tabs started, restarted or restored afterwards, and to the next `claude` started in a terminal tab. The page reads the stored value every time it opens, so it shows what new tabs will use. The Settings and Workspace settings tabs are not saved with the other tabs. If the stored settings cannot be read, Claude tabs start cautiously (Claude asks before running tools, and auto memory is off), a notice says so, and the page shows the error and cannot change them. Without a database (see Troubleshooting) settings last only until raterm quits.
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
- **Going back to an older raterm** after a newer one has opened the database (for example one with the workspace setting) makes the older one refuse the database and run without saving tabs. Move `raterm.db` aside or upgrade again.
- **After upgrading raterm**, restart it. A new version may change the database schema, and the session hook (which runs the new binary) refuses a schema it does not know until the app has been restarted and migrated it.
- **Diagnostics.** The backend prints to stderr with a `raterm:` prefix; start `raterm` from a terminal to see them.
