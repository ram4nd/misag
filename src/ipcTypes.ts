// The values the page and the backend exchange, mirroring their Rust types.
import type { FinishSound } from "./sounds";

/**
 * What a tab runs, tagged by `kind`; each kind has only its own fields.
 * Mirrors `layout::TabKind` in src-tauri/src/layout.rs.
 */
export type TabKind =
  | { kind: "terminal"; command: string | null; claudeSession?: string | null }
  | { kind: "claude"; claudeSession: string | null };

/**
 * A tab kind once a session has been assigned: a Claude tab always has one. A
 * terminal tab's `claudeSession` is the Claude session to resume at its prompt
 * when it starts (a restored tab that had one open), and null once handed over.
 * Mirrors `commands::RunningKind` in src-tauri/src/commands.rs.
 */
export type RunningKind =
  | { kind: "terminal"; command: string | null; claudeSession: string | null }
  | { kind: "claude"; claudeSession: string };

/** Mirrors `commands::TabLaunch` in src-tauri/src/commands.rs. */
export type TabLaunch = RunningKind & {
  tabId: string;
  cols: number;
  rows: number;
  cwd: string | null;
  fallbackToHome: boolean;
};

/** Mirrors `commands::Spawned` in src-tauri/src/commands.rs. */
export interface Spawned {
  id: number;
  fellBackToHome: boolean;
  /** The session hook reports for this tab (a Claude tab with a database). */
  tracked: boolean;
  /** Unix seconds, for `claude_reported`. */
  startedAt: number;
  skipsPermissions: boolean;
  resume: ResumeReport | null;
}

/** Mirrors `commands::ResumeReport` in src-tauri/src/commands.rs: pty_spawn gives queued, noConversation or failed, then a queued one gets typed or failed on its onResume channel. */
export type ResumeReport =
  | { state: "queued" }
  | { state: "typed" }
  | { state: "noConversation" }
  | { state: "failed"; message: string };

/** Mirrors `commands::ClaudeState` in src-tauri/src/commands.rs. */
export type ClaudeState =
  | { state: "open"; session: string; skipsPermissions: boolean }
  | { state: "closed" }
  | { state: "untracked"; pid: number; skipsPermissions: boolean }
  | { state: "unknown" };

/** Mirrors `db::StoreStatus` in src-tauri/src/db/mod.rs. */
export type StoreStatus =
  | { state: "saving" }
  | { state: "recovered"; notice: string }
  | { state: "notSaving"; reason: string };

/** Mirrors `claude::HookProblem` in src-tauri/src/claude/hook_log.rs. */
export interface HookProblem {
  /** Identifies the failure, so a dismissed one stays dismissed. */
  at: number;
  message: string;
  /** The hook log's path. */
  log: string;
}

/** Mirrors `commands::StoreReport` in src-tauri/src/commands.rs. */
export interface StoreReport {
  store: StoreStatus;
  hookProblem: HookProblem | null;
  /** Why the `claude` wrapper for terminal tabs could not be installed. */
  wrapperProblem: string | null;
  /** Why the `misag` shim for tabs could not be installed. */
  cliProblem: string | null;
}

/** Mirrors `layout::Tab` in src-tauri/src/layout.rs. */
export type LayoutTab = TabKind & { tabId: string; name: string; renamed: boolean; cwd: string | null };

/** Mirrors `layout::Layout` in src-tauri/src/layout.rs. */
export interface Layout {
  tabs: LayoutTab[];
  active: number;
}

/** Mirrors `settings::Settings` in src-tauri/src/settings.rs. */
export interface Settings {
  /** Start Claude tabs with `--dangerously-skip-permissions`. */
  skipPermissions: boolean;
  /** The workspace folder the sidebar shows; null for the home directory. */
  workspace: string | null;
  /** When true, Claude tabs get `autoMemoryEnabled: false`; when false, nothing is passed and the user's Claude config decides. */
  disableAutoMemory: boolean;
  /** The terminals' font size in pixels. */
  fontSize: number;
  /** The sound played when Claude in a tab finishes its turn. */
  finishSound: FinishSound;
}

/** The terminals' font size, and the range allowed: mirrors `settings::DEFAULT_FONT_SIZE` and `FONT_SIZES`. */
export const DEFAULT_FONT_SIZE = 14;
export const FONT_SIZES = { min: 8, max: 32 };

/**
 * Backend commands that can fail send `{ code, message }` (see `commands::CommandError`);
 * only the message is shown. Anything else (e.g. Tauri's own argument errors) is shown as is.
 */
export function errorText(err: unknown): string {
  return typeof err === "object" && err !== null && "message" in err ? String(err.message) : String(err);
}
