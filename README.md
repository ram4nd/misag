# raterm

A minimal Linux terminal: Tauri 2, a Rust PTY backend (`portable-pty`) and xterm.js, with Chrome-style tabs.

## Prerequisites

- Rust (stable) and Node.js with pnpm
- Tauri's Linux system libraries: `webkit2gtk-4.1`, `gtk3`, `libayatana-appindicator` (or `libappindicator-gtk3`), `librsvg`. See https://tauri.app/start/prerequisites/

## Commands

```sh
pnpm install
pnpm tauri dev      # run with hot reload
pnpm tauri build    # release build, produces .deb and AppImage bundles
pnpm test           # frontend tests (Vitest, jsdom, mocked Tauri IPC)
cargo test --manifest-path src-tauri/Cargo.toml   # PTY session tests against real processes
```

## Usage

| Action | How |
| --- | --- |
| New tab (login shell in `$HOME`) | Click **+** |
| New tab with name, directory or command | Shift+click **+** |
| Rename a tab | Double-click it |
| Close a tab | Click ✕ or middle-click the tab; closing the last tab closes the window |
| Open a link | Click a URL in the terminal (opens in the system browser) |
| Toggle the sidebar | Burger button at the left of the tab bar |

## Behaviour

- Sessions run `$SHELL -l` (falling back to `/bin/sh`), or `$SHELL -lc '<command>'` when a command is given. A leading `~` in the directory is expanded.
- Closing a tab sends SIGHUP and, if the process is still alive after 2 seconds, SIGKILL.
- When a process exits, its tab is struck through and shows the exit code. Tabs are not restored across restarts.
- Backend diagnostics are printed to stderr with a `raterm:` prefix.
