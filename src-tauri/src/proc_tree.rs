//! Which programs run in a tab, told from a list of processes and their parents, for
//! Windows, which has no foreground process group to ask. Kept free of Windows calls
//! so it is tested everywhere.

/// One process in a snapshot of the running processes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    pub parent: u32,
    /// The executable's file name, such as `claude.exe`.
    pub exe: String,
}

/// `exe` without its `.exe`, as Unix names a process.
pub fn base_name(exe: &str) -> &str {
    match exe.len().checked_sub(4) {
        Some(at) if exe.is_char_boundary(at) && exe[at..].eq_ignore_ascii_case(".exe") => &exe[..at],
        _ => exe,
    }
}

/// Programs Windows runs alongside a console program, never what a person started.
const CONSOLE_HOSTS: &[&str] = &["conhost", "openconsole"];

/// Names Claude Code runs under: its own binary, or node for an npm install.
const CLAUDE_NAMES: &[&str] = &["claude", "node"];

/// The programs that may be what runs in the foreground of the tab whose shell is
/// `shell`, most likely first: every `claude` or `node` below the shell, deepest
/// first (the `claude` wrapper, itself `claude.exe`, starts the real one below it),
/// then the shell's other children, the last listed first. Empty when the shell runs
/// nothing. The caller tells which one is Claude by asking who reported for the tab.
pub fn foreground_candidates(shell: u32, all: &[Process]) -> Vec<u32> {
    let named = |p: &Process, names: &[&str]| names.iter().any(|n| base_name(&p.exe).eq_ignore_ascii_case(n));
    // Breadth first, so depth is known; `seen` stops at cycles a reused pid can make.
    let mut seen = vec![shell];
    let mut level = vec![shell];
    let mut below: Vec<(usize, &Process)> = Vec::new();
    let mut depth = 0;
    while !level.is_empty() {
        depth += 1;
        let mut next = Vec::new();
        for p in all.iter().filter(|p| level.contains(&p.parent) && !named(p, CONSOLE_HOSTS)) {
            if !seen.contains(&p.pid) {
                seen.push(p.pid);
                next.push(p.pid);
                below.push((depth, p));
            }
        }
        level = next;
    }
    let mut claude: Vec<(usize, &Process)> = below.iter().copied().filter(|(_, p)| named(p, CLAUDE_NAMES)).collect();
    // Deepest first; among equals the one listed last first.
    claude.reverse();
    claude.sort_by_key(|(depth, _)| std::cmp::Reverse(*depth));
    let mut candidates: Vec<u32> = claude.iter().map(|(_, p)| p.pid).collect();
    for (_, p) in below.iter().rev().filter(|(d, _)| *d == 1) {
        if !candidates.contains(&p.pid) {
            candidates.push(p.pid);
        }
    }
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: u32, parent: u32, exe: &str) -> Process {
        Process { pid, parent, exe: exe.into() }
    }

    #[test]
    fn the_real_claude_comes_before_its_wrapper() {
        let all = [p(1, 0, "pwsh.exe"), p(2, 1, "conhost.exe"), p(3, 1, "claude.exe"), p(4, 3, "claude.exe")];
        assert_eq!(foreground_candidates(1, &all), vec![4, 3]);
    }

    #[test]
    fn an_npm_claude_runs_as_node_under_the_wrapper() {
        // The wrapper starts claude.cmd, which cmd.exe runs, which starts node.
        let all = [p(1, 0, "pwsh.exe"), p(3, 1, "claude.exe"), p(5, 3, "cmd.exe"), p(6, 5, "node.exe")];
        assert_eq!(foreground_candidates(1, &all), vec![6, 3]);
    }

    #[test]
    fn otherwise_the_shells_children_last_listed_first() {
        assert!(foreground_candidates(1, &[p(1, 0, "pwsh.exe"), p(2, 1, "conhost.exe")]).is_empty(), "just the shell");
        assert!(foreground_candidates(1, &[p(1, 0, "pwsh.exe"), p(2, 1, "OpenConsole.exe")]).is_empty());
        let all = [p(1, 0, "pwsh.exe"), p(2, 1, "git.exe"), p(3, 1, "vim.exe"), p(4, 3, "less.exe")];
        assert_eq!(foreground_candidates(1, &all), vec![3, 2]);
        assert!(foreground_candidates(1, &[p(9, 7, "claude.exe")]).is_empty(), "another tab's");
    }

    #[test]
    fn a_cycle_from_reused_pids_ends() {
        let all = [p(1, 0, "pwsh.exe"), p(2, 1, "a.exe"), p(3, 2, "b.exe"), p(2, 3, "a.exe"), p(1, 3, "x.exe")];
        assert_eq!(foreground_candidates(1, &all), vec![2]);
    }

    #[test]
    fn strips_exe() {
        assert_eq!(base_name("claude.exe"), "claude");
        assert_eq!(base_name("Claude.EXE"), "Claude");
        assert_eq!(base_name("node"), "node");
        assert_eq!(base_name("exe"), "exe");
        assert_eq!(base_name("añoé"), "añoé", "not cut inside a character");
    }
}
