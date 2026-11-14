//! What Windows needs in place of Unix process groups, signals and `/proc`: a job
//! object per tab, the process tree from a ToolHelp snapshot, and another process's
//! directory and command line from its process environment block.

use std::ffi::{c_void, OsString};
use std::io::{Error, ErrorKind};
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, INVALID_HANDLE_VALUE, STILL_ACTIVE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
    TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA,
    PROCESS_TERMINATE, PROCESS_VM_READ,
};

/// A handle closed when dropped.
struct Owned(HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: the handle is owned and closed once.
        unsafe { CloseHandle(self.0) };
    }
}

fn open(pid: u32, access: u32) -> std::io::Result<Owned> {
    // SAFETY: plain call; a null result is checked.
    let handle = unsafe { OpenProcess(access, 0, pid) };
    if handle.is_null() { Err(Error::last_os_error()) } else { Ok(Owned(handle)) }
}

/// A tab's processes: its shell and everything started under it, so closing the
/// tab ends them all (as a Unix process group does) and so does misag exiting.
pub struct Job(Owned);

// SAFETY: a job handle may be used from any thread.
unsafe impl Send for Job {}
unsafe impl Sync for Job {}

impl Job {
    /// A job holding process `pid`, killed as a whole once misag lets go of it.
    pub fn for_process(pid: u32) -> std::io::Result<Job> {
        // SAFETY: plain calls; results are checked.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err(Error::last_os_error());
            }
            let job = Owned(job);
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let size = std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32;
            if SetInformationJobObject(job.0, JobObjectExtendedLimitInformation, (&raw const limits).cast(), size) == 0 {
                return Err(Error::last_os_error());
            }
            let process = open(pid, PROCESS_SET_QUOTA | PROCESS_TERMINATE)?;
            if AssignProcessToJobObject(job.0, process.0) == 0 {
                return Err(Error::last_os_error());
            }
            Ok(Job(job))
        }
    }

    /// Ends every process in the job.
    pub fn terminate(&self) {
        // SAFETY: plain call on an owned handle.
        if unsafe { TerminateJobObject(self.0 .0, 1) } == 0 {
            eprintln!("misag: cannot end a tab's processes: {}", Error::last_os_error());
        }
    }
}

/// Whether process `pid` is still running.
pub fn alive(pid: u32) -> bool {
    let process = match open(pid, PROCESS_QUERY_LIMITED_INFORMATION) {
        Ok(process) => process,
        // Access denied means it exists, as someone else's.
        Err(e) => return e.raw_os_error() == Some(5),
    };
    let mut code = 0u32;
    // SAFETY: plain call on an owned handle.
    unsafe { GetExitCodeProcess(process.0, &mut code) != 0 && code == STILL_ACTIVE as u32 }
}

/// One process in a snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct Process {
    pub pid: u32,
    pub parent: u32,
    /// The executable's file name, such as `claude.exe`.
    pub exe: String,
}

/// Every running process.
pub fn processes() -> std::io::Result<Vec<Process>> {
    // SAFETY: plain calls on a snapshot this function owns; the entry size is set.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(Error::last_os_error());
        }
        let snapshot = Owned(snapshot);
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut all = Vec::new();
        let mut more = Process32FirstW(snapshot.0, &mut entry) != 0;
        while more {
            let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
            all.push(Process {
                pid: entry.th32ProcessID,
                parent: entry.th32ParentProcessID,
                exe: String::from_utf16_lossy(&entry.szExeFile[..len]),
            });
            more = Process32NextW(snapshot.0, &mut entry) != 0;
        }
        Ok(all)
    }
}

/// `exe` without its `.exe`, as Unix names a process.
pub fn base_name(exe: &str) -> &str {
    exe.len().checked_sub(4).filter(|&at| exe[at..].eq_ignore_ascii_case(".exe")).map_or(exe, |at| &exe[..at])
}

/// Programs Windows runs alongside a console program, never what a person started.
const CONSOLE_HOSTS: &[&str] = &["conhost", "openconsole"];

/// What runs in the foreground of the tab whose shell is `shell`, as Unix's foreground
/// process group tells it: the last `claude` found under the shell (the `claude`
/// wrapper, itself named `claude.exe`, starts the real one below it), else the last
/// `node`, else the newest program the shell started, else None.
pub fn foreground(shell: u32, all: &[Process]) -> Option<u32> {
    let is_host = |p: &Process| CONSOLE_HOSTS.iter().any(|h| base_name(&p.exe).eq_ignore_ascii_case(h));
    let mut below = vec![shell];
    let mut descendants = Vec::new();
    while let Some(parent) = below.pop() {
        for p in all.iter().filter(|p| p.parent == parent && p.pid != shell && !is_host(p)) {
            if !descendants.iter().any(|d: &&Process| d.pid == p.pid) {
                descendants.push(p);
                below.push(p.pid);
            }
        }
    }
    // Claude installed through npm runs as node.
    for name in ["claude", "node"] {
        if let Some(claude) = descendants.iter().rev().find(|p| base_name(&p.exe).eq_ignore_ascii_case(name)) {
            return Some(claude.pid);
        }
    }
    all.iter().filter(|p| p.parent == shell && !is_host(p)).map(|p| p.pid).last()
}

/// The name of process `pid` without `.exe`, such as `claude`.
pub fn process_name(pid: u32) -> Option<String> {
    let all = processes().ok()?;
    all.into_iter().find(|p| p.pid == pid).map(|p| base_name(&p.exe).to_owned())
}

/// `PROCESS_BASIC_INFORMATION`, declared here so only its layout is needed.
#[repr(C)]
struct BasicInformation {
    exit_status: i32,
    peb: usize,
    affinity: usize,
    priority: i32,
    pid: usize,
    parent: usize,
}

/// Offsets in a 64-bit process's environment block (PEB) and its
/// `RTL_USER_PROCESS_PARAMETERS`, stable since Windows XP x64.
const PEB_PARAMETERS: usize = 0x20;
const PARAMETERS_CURRENT_DIRECTORY: usize = 0x38;
const PARAMETERS_COMMAND_LINE: usize = 0x70;

fn read<T: Copy>(process: &Owned, address: usize) -> std::io::Result<T> {
    let mut value = std::mem::MaybeUninit::<T>::uninit();
    let mut read = 0usize;
    let size = std::mem::size_of::<T>();
    // SAFETY: ReadProcessMemory writes at most `size` bytes into `value`.
    let ok = unsafe {
        windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory(
            process.0,
            address as *const c_void,
            value.as_mut_ptr().cast(),
            size,
            &mut read,
        )
    };
    if ok == 0 || read != size {
        return Err(Error::last_os_error());
    }
    // SAFETY: fully written above.
    Ok(unsafe { value.assume_init() })
}

/// A `UNICODE_STRING` in process `pid`'s parameters, at `offset`.
fn parameter(pid: u32, offset: usize) -> std::io::Result<Vec<u16>> {
    if cfg!(not(target_pointer_width = "64")) {
        return Err(Error::new(ErrorKind::Unsupported, "only 64-bit builds read other processes"));
    }
    let process = open(pid, PROCESS_QUERY_INFORMATION | PROCESS_VM_READ).map_err(|e| {
        // ERROR_INVALID_PARAMETER: no such process.
        if e.raw_os_error() == Some(87) { Error::new(ErrorKind::NotFound, e) } else { e }
    })?;
    let mut info: BasicInformation = unsafe { std::mem::zeroed() };
    let mut len = 0u32;
    // SAFETY: the kernel fills at most the given size.
    let status = unsafe {
        windows_sys::Wdk::System::Threading::NtQueryInformationProcess(
            process.0,
            0, // ProcessBasicInformation
            (&raw mut info).cast(),
            std::mem::size_of::<BasicInformation>() as u32,
            &mut len,
        )
    };
    if status < 0 || info.peb == 0 {
        return Err(Error::other(format!("cannot query process {pid} (status {status:#x})")));
    }
    let parameters: usize = read(&process, info.peb + PEB_PARAMETERS)?;
    // UNICODE_STRING: length in bytes (u16), maximum length (u16), padding, buffer.
    let length: u16 = read(&process, parameters + offset)?;
    let buffer: usize = read(&process, parameters + offset + 8)?;
    let mut text = vec![0u16; usize::from(length) / 2];
    if !text.is_empty() {
        let mut got = 0usize;
        // SAFETY: writes at most `length` bytes into `text`, which holds that many.
        let ok = unsafe {
            windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory(
                process.0,
                buffer as *const c_void,
                text.as_mut_ptr().cast(),
                usize::from(length),
                &mut got,
            )
        };
        if ok == 0 {
            return Err(Error::last_os_error());
        }
    }
    Ok(text)
}

/// Process `pid`'s current directory.
pub fn cwd(pid: u32) -> std::io::Result<PathBuf> {
    let mut dir = parameter(pid, PARAMETERS_CURRENT_DIRECTORY)?;
    // Windows keeps it with a trailing backslash, which only a drive's root needs.
    if dir.len() > 3 && dir.last() == Some(&u16::from(b'\\')) {
        dir.pop();
    }
    Ok(PathBuf::from(OsString::from_wide(&dir)))
}

/// Process `pid`'s arguments, split the way programs split their command line.
pub fn args(pid: u32) -> Option<Vec<String>> {
    let mut line = parameter(pid, PARAMETERS_COMMAND_LINE).ok()?;
    if line.is_empty() {
        return Some(Vec::new());
    }
    line.push(0);
    let mut count = 0i32;
    // SAFETY: `line` is NUL-terminated; the returned array is freed below.
    unsafe {
        let argv = windows_sys::Win32::UI::Shell::CommandLineToArgvW(line.as_ptr(), &mut count);
        if argv.is_null() {
            return None;
        }
        let args = (0..usize::try_from(count).unwrap_or(0))
            .map(|i| {
                let arg = *argv.add(i);
                let len = (0..).take_while(|&j| *arg.add(j) != 0).count();
                String::from_utf16_lossy(std::slice::from_raw_parts(arg, len))
            })
            .collect();
        LocalFree(argv.cast());
        Some(args)
    }
}

/// The clipboard's text; None if it holds none.
pub fn clipboard_text() -> Result<Option<String>, String> {
    use windows_sys::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard};
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    const CF_UNICODETEXT: u32 = 13;
    // SAFETY: the clipboard is opened and closed here; the locked text is NUL-terminated
    // UTF-16 and copied out before it is unlocked.
    unsafe {
        if IsClipboardFormatAvailable(CF_UNICODETEXT) == 0 {
            return Ok(None);
        }
        // Another program may hold it for a moment.
        let mut opened = false;
        for _ in 0..10 {
            if OpenClipboard(std::ptr::null_mut()) != 0 {
                opened = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if !opened {
            return Err(format!("cannot open the clipboard: {}", Error::last_os_error()));
        }
        let data = GetClipboardData(CF_UNICODETEXT);
        let text = if data.is_null() {
            None
        } else {
            let locked = GlobalLock(data).cast::<u16>();
            if locked.is_null() {
                None
            } else {
                let len = (0..).take_while(|&i| *locked.add(i) != 0).count();
                let text = String::from_utf16_lossy(std::slice::from_raw_parts(locked, len));
                GlobalUnlock(data);
                Some(text)
            }
        };
        CloseClipboard();
        Ok(text.filter(|t| !t.is_empty()))
    }
}

/// Keeps this process alive through Ctrl+C while a program it started runs in the
/// same console: Windows sends the key to every process there, and the program
/// decides what it means (Claude interrupts its turn).
pub fn outlive_ctrl_c() {
    unsafe extern "system" fn handled(_: u32) -> windows_sys::core::BOOL {
        1
    }
    // SAFETY: registers a handler that only returns.
    unsafe { windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(handled), 1) };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: u32, parent: u32, exe: &str) -> Process {
        Process { pid, parent, exe: exe.into() }
    }

    #[test]
    fn finds_claude_below_the_shell() {
        let all = [p(1, 0, "pwsh.exe"), p(2, 1, "conhost.exe"), p(3, 1, "claude.exe"), p(4, 3, "misag-cli.exe"), p(5, 4, "claude.exe")];
        assert_eq!(foreground(1, &all), Some(5), "the real claude under the wrapper");
        let direct = [p(1, 0, "pwsh.exe"), p(3, 1, "Claude.EXE"), p(6, 1, "git.exe")];
        assert_eq!(foreground(1, &direct), Some(3));
    }

    #[test]
    fn otherwise_the_newest_child_or_none() {
        assert_eq!(foreground(1, &[p(1, 0, "pwsh.exe"), p(2, 1, "conhost.exe")]), None, "just the shell");
        assert_eq!(foreground(1, &[p(1, 0, "pwsh.exe"), p(2, 1, "git.exe"), p(3, 1, "vim.exe")]), Some(3));
        assert_eq!(foreground(1, &[p(9, 7, "claude.exe")]), None, "another tab's");
    }

    #[test]
    fn strips_exe() {
        assert_eq!(base_name("claude.exe"), "claude");
        assert_eq!(base_name("Claude.EXE"), "Claude");
        assert_eq!(base_name("node"), "node");
        assert_eq!(base_name("exe"), "exe");
    }
}
