<p align="center"><img src="app-icon.png" width="160" alt="A kratt made of sticks with glowing eyes"></p>

# Mischief of Agents (`misag`)

A terminal for Linux, macOS and Windows for keeping AI agents on the path you set.

Left alone, coding agents like Claude Code decide for themselves what to do next, and what they remember between sessions steers them further. Mischief of Agents puts you back in charge: turn Claude's auto memory off, and give it a flow, a graph of steps you draw, that says what to do at each step and where to go next. Claude works through it on its own and comes back to you where you said it should, or when something goes wrong.

It also works as a lightweight IDE for your agents without any flows: open a tab per agent, browse your workspace, and pick up every conversation where it left off after a restart.

The name and logo come from the same idea. A *mischief* is a group of rats, a nod to the project's old name, raterm. The logo is a *kratt*, a creature from Estonian folklore that its master builds from sticks and household junk and brings to life to work for them. It must always be kept busy, or it turns to mischief.

## Features

- **Flows.** Draw a graph of prompt steps and manual steps on a canvas, then run it in a Claude tab. Claude ends each reply with the output it chose (`FLOW: yes`), and the run moves on to the next step by itself, looping for review and fix rounds with a limit on how often a step runs, and waiting for you at manual steps or when something goes wrong.
- **Control over every Claude it starts.** Turn auto memory off and skip permission prompts for Claude tabs and for `claude` in terminal tabs alike, without touching your own Claude config.
- **Tabs that remember Claude.** Claude tabs and terminal tabs reopen after a restart, and Claude comes back in the conversation it was last in, including after `/clear` and `/resume`. A `claude` started at a terminal tab's prompt is tracked too.
- **Session history.** See every Claude conversation you have saved, in any folder, resume one in a new tab, and pin the ones you keep coming back to.
- **Workspace browser.** Browse a folder tree and click a file, or drag files in from your file manager, to type its path into the active tab.
- Chrome-style tabs that follow your `cd`, Ctrl+clickable links, and one window at a time.

Built with Tauri 2, a Rust PTY backend (`portable-pty`), wterm and SQLite.

## Install

Each GitHub Release has a Linux `.deb` and AppImage, an Apple Silicon macOS `.dmg` and a Windows `.msi`. The macOS build is signed and notarized by Apple. On Windows, tabs run PowerShell (PowerShell 7 when installed).

To build it from source instead (on Linux) you need:

- Rust (stable), Node.js and pnpm
- Tauri's system libraries: `webkit2gtk-4.1`, `gtk3`, `libayatana-appindicator` (or `libappindicator-gtk3`) and `librsvg`. See https://tauri.app/start/prerequisites/
- [Claude Code](https://claude.com/claude-code) on your login shell's `PATH`, for Claude tabs and flows

```sh
pnpm install
pnpm tauri build
```

The `.deb` and AppImage bundles land in `src-tauri/target/release/bundle/`. Install one, or run `src-tauri/target/release/misag` directly.

## Quick start

- Click the terminal icon at the top right for a new terminal tab, or Shift+click it to pick a name, a folder and whether it runs Claude.
- Hover a folder in the Workspace box and click its Claude icon to start Claude there.
- Open the Flows box's gear to make a flow, click the flow to edit its steps, then hover it and click ▶ with a Claude tab selected.
- Flows export and import as JSON files on the Flows tab, or with `misag flow` in a terminal. Ask Claude in a misag tab to "run `misag flow help`, then add a misag flow that …" and it writes one.

The [reference](docs/reference.md) covers every action, how each feature behaves and troubleshooting.

## Development

```sh
pnpm app:dev        # run with hot reload, as "Mischief of Agents Dev" with its own data
pnpm test           # frontend tests (Vitest, jsdom, mocked Tauri IPC)
cargo test --manifest-path src-tauri/Cargo.toml   # backend: sessions, database, hook, exec (runs real processes)
# Opt-in: drives a real Claude (checks /clear and resume; the resume test sends one short prompt).
# Needs a folder Claude already trusts.
MISAG_LIVE_DIR=~/some/trusted/project cargo test --manifest-path src-tauri/Cargo.toml --test claude_live -- --ignored
```

The logo's source is `app-icon.svg`. After changing it, render `app-icon.png` at 1024px and regenerate the app icons with `pnpm tauri icon app-icon.png`.

## License

Copyright (C) 2026 Ra Mänd. Licensed under the GNU General Public License v3.0 only (GPL-3.0-only); see [LICENSE](LICENSE). Other licensing terms are available from the copyright holder.

Mischief of Agents is an independent project, not affiliated with or endorsed by Anthropic. Claude and Claude Code are trademarks of Anthropic.
