/** Whether misag runs on Windows, where tabs run PowerShell and paths use backslashes. */
export const WINDOWS = typeof navigator !== "undefined" && /Windows/.test(navigator.userAgent);

/**
 * Shell-quotes `s` unless it is plain enough to type as is: for a POSIX shell, or on
 * Windows for PowerShell, whose single quotes take everything but a single quote,
 * which is written twice. PowerShell also reads the typographic quotes ‘ ’ ‚ ‛ as
 * single quotes, and Windows file names may hold them.
 */
export function shellQuote(s: string, windows = WINDOWS): string {
  if (windows) return /^[\w%+=:./\\-]+$/.test(s) ? s : `'${s.replace(/['\u2018\u2019\u201a\u201b]/g, "$&$&")}'`;
  return /^[\w@%+=:,./-]+$/.test(s) ? s : `'${s.replace(/'/g, "'\\''")}'`;
}

/**
 * Why `path` cannot be typed into a terminal safely, or null if it can. Control
 * characters act as keys (Ctrl-C, Enter, Escape) even inside quotes, and a
 * backslash changes how fish reads single quotes (PowerShell's take it as is).
 */
export function untypeable(path: string, windows = WINDOWS): string | null {
  if (/[\x00-\x1f\x7f-\x9f]/.test(path)) return "its name contains control characters";
  if (!windows && path.includes("\\")) return "its name contains a backslash";
  return null;
}

/**
 * Why dropped `path` is not a local file's path, or null if it is. The webview passes
 * links and files on remote locations (on Windows, network shares) on as they are,
 * and reads names that are not UTF-8 lossily.
 */
export function notLocal(path: string, windows = WINDOWS): string | null {
  const local = windows ? /^[A-Za-z]:[\\/]/.test(path) : path.startsWith("/");
  if (!local) return "it is not a local file";
  if (path.includes("\ufffd")) return "its name could not be read";
  return null;
}

/**
 * What a tab shows for directory `path`: its last folder, `~` for `home` and the root
 * itself (`/`, or a drive such as `C:\` on Windows) for a root.
 */
export function folderLabel(path: string, home: string | null, windows = WINDOWS): string {
  const separators = windows ? /[\\/]+$/ : /\/+$/;
  const trimmed = path.replace(separators, "");
  if (!trimmed) return "/";
  if (windows && /^[A-Za-z]:$/.test(trimmed)) return `${trimmed}\\`;
  // Windows paths ignore case.
  const same = (a: string, b: string) => (windows ? a.toLowerCase() === b.toLowerCase() : a === b);
  if (home !== null && same(trimmed, home.replace(separators, ""))) return "~";
  const last = windows ? Math.max(trimmed.lastIndexOf("/"), trimmed.lastIndexOf("\\")) : trimmed.lastIndexOf("/");
  return trimmed.slice(last + 1);
}
