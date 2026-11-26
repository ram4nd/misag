<p align="center"><img src="app-icon.png" width="160" alt="A kratt made of sticks with glowing eyes"></p>

# Mischief of Agents (`misag`)

A desktop terminal for Linux, macOS and Windows that keeps Claude Code on the path you set.

Left alone, a coding agent decides for itself what to do next, and what it remembers between sessions steers it further. misag puts you back in charge. You draw a **flow**, a graph of steps that says what Claude does at each step and where to go next. Claude works through it on its own in a terminal tab and comes back to you where you said it should, or when something goes wrong.

A flow is not a pipeline of API calls like Langflow or n8n. It steers one real, interactive Claude Code conversation: each step is a prompt to that conversation, Claude picks which way the flow goes, and you can watch, type into it, or step in at any point.

Without flows, misag is still a good home for your agents: a tab per agent, a workspace browser, and every conversation back where it left off after a restart.

## How a flow works

You build a flow on a canvas from a few kinds of steps:

- **Start**, with optional base info (where things are, what Claude should know), and an option to begin each run with `/clear` in a fresh conversation.
- **Prompt**, sent to Claude. A prompt step can also be yours to write ("Write it myself": its text is put into Claude's input as a draft for you to finish and send), and it can pause when done ("Pause when done": the run waits for you, with Claude's choice ready to accept with ▶, and an optional note on what to check). A pause with no prompt only waits for you.
- **Tab title**, where Claude names the tab after what you asked it.
- **End**, though an output that leads nowhere ends the flow too.

Each step has outputs, short words such as `pass, fail`, and each output leads to the next step, an earlier one included, which makes loops like review, fix, review again. A step can have a run limit, so a loop cannot run forever.

When you press ▶, misag briefs Claude on the rules, then sends the steps one at a time. Claude ends each reply with the output it picked (`FLOW: pass`), and misag's Claude Code hooks move the run on by themselves. The run stops for you when a step pauses or is yours to write, when Claude asks for you (`FLOW: attention`), or when a reply cannot be followed. You can always pick an output yourself, have Claude go on, or stop the run.

Flows export and import as JSON files. Claude can write them too: ask it in a misag tab to "run `misag flow help`, then add a misag flow that …".

## Features

- **Flows**, as above, with a run bar in the flow's editor that shows where the run is, why it waits, and the ways on.
- **Control over every Claude it starts.** Turn Claude's auto memory off and skip permission prompts for Claude tabs and for `claude` in terminal tabs alike, without touching your own Claude config.
- **Tabs that remember Claude.** Claude tabs and terminal tabs reopen after a restart, and Claude comes back in the conversation it was last in, including after `/clear` and `/resume`. A `claude` started at a terminal tab's prompt is tracked too.
- **See what Claude is doing.** A tab's icon spins while Claude works or waits on background tasks, a tab with a run waiting for you gets an orange dot, and an optional sound plays when Claude finishes.
- **Session history.** Every Claude conversation you have saved, in any folder: resume one in a new tab, and pin the ones you come back to.
- **Workspace browser.** Browse a folder tree, and click a file or drag files in from your file manager to type their paths into the active tab.
- Chrome-style tabs that follow your `cd`, Ctrl+clickable links, and one window at a time.

Built with Tauri 2, a Rust PTY backend (`portable-pty`), wterm and SQLite.

## Install

Each GitHub Release has a Linux `.deb` and AppImage, an Apple Silicon macOS `.dmg` (signed and notarized) and a Windows `.msi` (tabs run PowerShell, version 7 when installed). You need [Claude Code](https://claude.com/claude-code) on your login shell's `PATH` for Claude tabs and flows.

### From source

You need Rust (stable), Node.js, pnpm, and Tauri's system libraries (`webkit2gtk-4.1`, `gtk3`, `libayatana-appindicator` or `libappindicator-gtk3`, and `librsvg`; see https://tauri.app/start/prerequisites/).

On Arch Linux, this one line builds a package from the checkout and installs it; run it again to update:

```sh
pnpm install && packaging/arch/build.sh -si
```

Elsewhere, build the bundles and install one, or run `src-tauri/target/release/misag` directly:

```sh
pnpm install
pnpm tauri build    # bundles land in src-tauri/target/release/bundle/
```

Restart misag after an update: a new version may migrate its database, and its Claude hook waits for that.

## Quick start

1. Click the terminal icon at the top right for a new terminal tab, or Shift+click it to pick a name, a folder and whether it runs Claude. Or hover a folder in the Workspace box and click its Claude icon.
2. Open the Flows box's gear to make a flow, then click the flow to draw its steps.
3. Select a Claude tab, hover the flow and click ▶. When the run waits for you, ▶ beside ■ goes on.

The [reference](docs/reference.md) covers every action, how each feature behaves, and troubleshooting.

## Development

```sh
pnpm app:dev        # run with hot reload, as "Mischief of Agents Dev" with its own data
pnpm test           # frontend tests (Vitest, jsdom, mocked Tauri IPC)
cargo test --manifest-path src-tauri/Cargo.toml   # backend: sessions, database, hooks, flows (runs real processes)
# Opt-in: drives a real Claude (checks /clear and resume; the resume test sends one short prompt).
# Needs a folder Claude already trusts.
MISAG_LIVE_DIR=~/some/trusted/project cargo test --manifest-path src-tauri/Cargo.toml --test claude_live -- --ignored
```

CI (`.github/workflows/ci.yml`) runs these tests (except the live Claude ones), the build and `cargo clippy -- -D warnings` on every push to main and every pull request.

The logo's source is `app-icon.svg`. After changing it, render `app-icon.png` at 1024px and regenerate the app icons with `pnpm tauri icon app-icon.png`.

## The name

A *mischief* is a group of rats, a nod to the project's old name, raterm. The logo is a *kratt*, a creature from Estonian folklore that its master builds from sticks and household junk and brings to life to work for them. It must always be kept busy, or it turns to mischief.

## License

Copyright (C) 2026 Ra Mänd. Licensed under the GNU General Public License v3.0 only (GPL-3.0-only); see [LICENSE](LICENSE). Other licensing terms are available from the copyright holder.

Mischief of Agents is an independent project, not affiliated with or endorsed by Anthropic. Claude and Claude Code are trademarks of Anthropic.
