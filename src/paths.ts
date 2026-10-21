/** Shell-quotes `s` for a POSIX shell unless it is plain enough to type as is. */
export function shellQuote(s: string): string {
  return /^[\w@%+=:,./-]+$/.test(s) ? s : `'${s.replace(/'/g, "'\\''")}'`;
}

/**
 * Why `path` cannot be typed into a terminal safely, or null if it can. Control
 * characters act as keys (Ctrl-C, Enter, Escape) even inside quotes, and a
 * backslash changes how fish reads single quotes.
 */
export function untypeable(path: string): string | null {
  if (/[\x00-\x1f\x7f-\x9f]/.test(path)) return "its name contains control characters";
  if (path.includes("\\")) return "its name contains a backslash";
  return null;
}
