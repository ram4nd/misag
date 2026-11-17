//! What Windows needs in place of Unix process groups, signals and `/proc`: a job
//! object per tab, the process list from a ToolHelp snapshot, and another process's
//! directory and command line from its process environment block (PEB).

use crate::proc_tree::Process;
use std::ffi::{c_void, OsString};
use std::io::{Error, ErrorKind};
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    CloseHandle, LocalFree, ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, HANDLE, INVALID_HANDLE_VALUE, STILL_ACTIVE,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
    QueryInformationJobObject, SetInformationJobObject, TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, IsWow64Process, OpenProcess, PROCESS_BASIC_INFORMATION, PROCESS_QUERY_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA, PROCESS_TERMINATE, PROCESS_VM_READ,
};

/// A handle closed when dropped. Made only through `new`, which checks it.
struct Owned(HANDLE);

impl Owned {
    /// Takes `handle`, or the last error when it is null or `INVALID_HANDLE_VALUE`
    /// (Windows calls fail with one or the other).
    fn new(handle: HANDLE) -> std::io::Result<Owned> {
        if handle.is_null() || handle == INVALID_HANDLE_VALUE { Err(Error::last_os_error()) } else { Ok(Owned(handle)) }
    }

    fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: the handle is owned and closed once.
        unsafe { CloseHandle(self.0) };
    }
}

fn open(pid: u32, access: u32) -> std::io::Result<Owned> {
    // SAFETY: plain call; the result is checked.
    Owned::new(unsafe { OpenProcess(access, 0, pid) })
}

/// A tab's processes: its shell and everything started under it, so closing the
/// tab ends them all (as a Unix process group does), and so does misag exiting.
///
/// The shell joins the job just after it starts (portable-pty cannot start it
/// suspended), so a program a PowerShell profile starts in that instant can escape.
pub struct Job(Owned);

// SAFETY: a job handle may be used from any thread, and `Job` changes no Rust state.
unsafe impl Send for Job {}
unsafe impl Sync for Job {}

impl Job {
    /// A job holding process `pid`, killed as a whole once misag lets go of it.
    pub fn for_process(pid: u32) -> std::io::Result<Job> {
        // SAFETY: plain calls; results are checked.
        unsafe {
            let job = Owned::new(CreateJobObjectW(std::ptr::null(), std::ptr::null()))?;
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let size = std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32;
            if SetInformationJobObject(job.raw(), JobObjectExtendedLimitInformation, (&raw const limits).cast(), size) == 0 {
                return Err(Error::last_os_error());
            }
            let process = open(pid, PROCESS_SET_QUOTA | PROCESS_TERMINATE)?;
            if AssignProcessToJobObject(job.raw(), process.raw()) == 0 {
                return Err(Error::last_os_error());
            }
            Ok(Job(job))
        }
    }

    /// How many processes the job still holds.
    fn active(&self) -> std::io::Result<u32> {
        // SAFETY: the kernel fills at most the given size.
        unsafe {
            let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = std::mem::zeroed();
            let size = std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32;
            let ok = QueryInformationJobObject(
                self.0.raw(),
                JobObjectBasicAccountingInformation,
                (&raw mut info).cast(),
                size,
                std::ptr::null_mut(),
            );
            if ok == 0 { Err(Error::last_os_error()) } else { Ok(info.ActiveProcesses) }
        }
    }

    /// Gives the job's processes up to `grace` to exit (closing the tab closed their
    /// console, which asks console programs such as Claude to end), then ends the rest.
    pub fn end_within(&self, grace: Duration) {
        let until = Instant::now() + grace;
        while Instant::now() < until {
            match self.active() {
                Ok(0) => return,
                Ok(_) => std::thread::sleep(Duration::from_millis(50)),
                Err(e) => {
                    eprintln!("misag: cannot count a closed tab's processes, ending them now: {e}");
                    break;
                }
            }
        }
        // SAFETY: plain call on an owned handle.
        if unsafe { TerminateJobObject(self.0.raw(), 1) } == 0 {
            eprintln!("misag: cannot end a closed tab's processes: {}", Error::last_os_error());
        }
    }
}

/// Whether process `pid` may still be running. Only an answer that it has exited, or
/// that no such process exists, counts as gone; any other failure counts as running,
/// so a passing error never releases a live Claude session.
pub fn alive(pid: u32) -> bool {
    let process = match open(pid, PROCESS_QUERY_LIMITED_INFORMATION) {
        Ok(process) => process,
        Err(e) if e.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) => return false,
        Err(e) => {
            if e.raw_os_error() != Some(ERROR_ACCESS_DENIED as i32) {
                eprintln!("misag: cannot tell whether process {pid} runs, so it counts as running: {e}");
            }
            return true;
        }
    };
    let mut code = 0u32;
    // SAFETY: plain call on an owned handle.
    if unsafe { GetExitCodeProcess(process.raw(), &mut code) } == 0 {
        eprintln!("misag: cannot read the exit code of process {pid}, so it counts as running: {}", Error::last_os_error());
        return true;
    }
    code == STILL_ACTIVE as u32
}

/// Every running process.
pub fn processes() -> std::io::Result<Vec<Process>> {
    // SAFETY: plain calls on a snapshot this function owns; the entry size is set.
    unsafe {
        let snapshot = Owned::new(CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0))?;
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut all = Vec::new();
        let mut more = Process32FirstW(snapshot.raw(), &mut entry) != 0;
        while more {
            let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
            all.push(Process {
                pid: entry.th32ProcessID,
                parent: entry.th32ParentProcessID,
                exe: String::from_utf16_lossy(&entry.szExeFile[..len]),
            });
            more = Process32NextW(snapshot.raw(), &mut entry) != 0;
        }
        Ok(all)
    }
}

/// The name of process `pid` without `.exe`, such as `claude`.
pub fn process_name(pid: u32) -> Option<String> {
    let all = processes().map_err(|e| eprintln!("misag: cannot list processes: {e}")).ok()?;
    all.into_iter().find(|p| p.pid == pid).map(|p| crate::proc_tree::base_name(&p.exe).to_owned())
}

/// Values any bit pattern is valid for, which `read` may fill from another process.
trait Plain: Copy {}
impl Plain for u16 {}
impl Plain for usize {}

/// Reads a `T` at `address` in `process`, failing on a short read.
fn read<T: Plain>(process: &Owned, address: usize) -> std::io::Result<T> {
    let mut value = std::mem::MaybeUninit::<T>::uninit();
    let size = std::mem::size_of::<T>();
    read_into(process, address, value.as_mut_ptr().cast(), size)?;
    // SAFETY: fully written above, and every bit pattern is a valid `T`.
    Ok(unsafe { value.assume_init() })
}

/// Reads exactly `size` bytes at `address` in `process` into `buffer`.
fn read_into(process: &Owned, address: usize, buffer: *mut c_void, size: usize) -> std::io::Result<()> {
    let mut got = 0usize;
    // SAFETY: the caller's buffer holds `size` bytes; ReadProcessMemory writes at most that.
    let ok = unsafe {
        windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory(
            process.raw(),
            address as *const c_void,
            buffer,
            size,
            &mut got,
        )
    };
    if ok == 0 {
        return Err(Error::last_os_error());
    }
    if got != size {
        return Err(Error::new(ErrorKind::UnexpectedEof, format!("read {got} of {size} bytes")));
    }
    Ok(())
}

/// Offsets in a 64-bit process's PEB and its `RTL_USER_PROCESS_PARAMETERS`.
const PEB_PARAMETERS: usize = 0x20;
const PARAMETERS_CURRENT_DIRECTORY: usize = 0x38;
const PARAMETERS_COMMAND_LINE: usize = 0x70;

/// A `UNICODE_STRING` in process `pid`'s parameters, at `offset`.
fn parameter(pid: u32, offset: usize) -> std::io::Result<Vec<u16>> {
    if cfg!(not(target_pointer_width = "64")) {
        return Err(Error::new(ErrorKind::Unsupported, "only 64-bit builds read other processes"));
    }
    let process = open(pid, PROCESS_QUERY_INFORMATION | PROCESS_VM_READ).map_err(|e| {
        if e.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) { Error::new(ErrorKind::NotFound, e) } else { e }
    })?;
    // A 32-bit process's own PEB is laid out differently.
    let mut wow64 = 0;
    // SAFETY: plain call on an owned handle.
    if unsafe { IsWow64Process(process.raw(), &mut wow64) } == 0 {
        return Err(Error::last_os_error());
    }
    if wow64 != 0 {
        return Err(Error::new(ErrorKind::Unsupported, format!("process {pid} is a 32-bit program")));
    }
    // SAFETY: plain data the kernel fills, of the size it is told.
    let mut info: PROCESS_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    let mut len = 0u32;
    let status = unsafe {
        windows_sys::Wdk::System::Threading::NtQueryInformationProcess(
            process.raw(),
            windows_sys::Wdk::System::Threading::ProcessBasicInformation,
            (&raw mut info).cast(),
            std::mem::size_of::<PROCESS_BASIC_INFORMATION>() as u32,
            &mut len,
        )
    };
    let peb = info.PebBaseAddress as usize;
    if status < 0 || peb == 0 {
        return Err(Error::other(format!("cannot query process {pid} (status {status:#x})")));
    }
    let parameters: usize = read(&process, peb + PEB_PARAMETERS)?;
    // UNICODE_STRING: length in bytes (u16), maximum length (u16), padding, buffer.
    let length: u16 = read(&process, parameters + offset)?;
    let buffer: usize = read(&process, parameters + offset + 8)?;
    let mut text = vec![0u16; usize::from(length) / 2];
    if !text.is_empty() {
        read_into(&process, buffer, text.as_mut_ptr().cast(), text.len() * 2)?;
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

/// Process `pid`'s arguments, split the way programs split their command line; None
/// (logged) when they cannot be read, which callers must not take as "no flags".
pub fn args(pid: u32) -> Option<Vec<String>> {
    let mut line = match parameter(pid, PARAMETERS_COMMAND_LINE) {
        Ok(line) => line,
        Err(e) => {
            if e.kind() != ErrorKind::NotFound {
                eprintln!("misag: cannot read the command line of process {pid}: {e}");
            }
            return None;
        }
    };
    if line.is_empty() {
        return Some(Vec::new());
    }
    line.push(0);
    let mut count = 0i32;
    // SAFETY: `line` is NUL-terminated; the returned array is freed below.
    unsafe {
        let argv = windows_sys::Win32::UI::Shell::CommandLineToArgvW(line.as_ptr(), &mut count);
        if argv.is_null() {
            eprintln!("misag: cannot split the command line of process {pid}: {}", Error::last_os_error());
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
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    const CF_UNICODETEXT: u32 = 13;
    // SAFETY: the clipboard is opened and closed here; the locked text is read only
    // within its allocation's size and copied out before it is unlocked.
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
            std::thread::sleep(Duration::from_millis(20));
        }
        if !opened {
            return Err(format!("cannot open the clipboard: {}", Error::last_os_error()));
        }
        let data = GetClipboardData(CF_UNICODETEXT);
        let text = if data.is_null() {
            Err(format!("cannot read the clipboard: {}", Error::last_os_error()))
        } else {
            let locked = GlobalLock(data).cast::<u16>();
            if locked.is_null() {
                Err(format!("cannot read the clipboard: {}", Error::last_os_error()))
            } else {
                // It should end in a NUL, but the program that put it there may have
                // left that out; the allocation's size bounds it either way. A size of
                // 0 is an error only when the call says so.
                windows_sys::Win32::Foundation::SetLastError(0);
                let size = GlobalSize(data);
                let failed = (size == 0).then(Error::last_os_error).filter(|e| e.raw_os_error() != Some(0));
                if let Some(e) = failed {
                    GlobalUnlock(data);
                    Err(format!("cannot read the clipboard: {e}"))
                } else {
                    let units = std::slice::from_raw_parts(locked, size / 2);
                    let len = units.iter().position(|&c| c == 0).unwrap_or(units.len());
                    let text = String::from_utf16_lossy(&units[..len]);
                    GlobalUnlock(data);
                    Ok(text)
                }
            }
        };
        CloseClipboard();
        text.map(|t| (!t.is_empty()).then_some(t))
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
    if unsafe { windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(handled), 1) } == 0 {
        eprintln!("misag: cannot outlive Ctrl+C, which may end this wrapper before Claude: {}", Error::last_os_error());
    }
}

/// Sends this process's stderr to `file`, for a windowed app, which has none.
pub fn redirect_stderr(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Console::{SetStdHandle, STD_ERROR_HANDLE};
    // SAFETY: the handle stays valid while `file` lives, which the caller keeps for good.
    if unsafe { SetStdHandle(STD_ERROR_HANDLE, file.as_raw_handle()) } == 0 {
        return Err(Error::last_os_error());
    }
    Ok(())
}
