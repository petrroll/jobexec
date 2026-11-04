//! Windows implementation: Job Object with KILL_ON_JOB_CLOSE around a suspended-then-resumed child.

use std::io;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, DuplicateHandle, DUPLICATE_SAME_ACCESS, ERROR_FILE_NOT_FOUND,
    ERROR_PATH_NOT_FOUND, FALSE, FILETIME, HANDLE, INVALID_HANDLE_VALUE, TRUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::System::Console::{
    GetStdHandle, SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_C_EVENT, STD_ERROR_HANDLE,
    STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Environment::GetCommandLineW;
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectAssociateCompletionPortInformation,
    JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
    QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
    JOBOBJECT_ASSOCIATE_COMPLETION_PORT, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, GetCurrentProcess, GetCurrentProcessId, GetExitCodeProcess, GetProcessTimes,
    OpenProcess, ResumeThread, TerminateProcess, WaitForSingleObject, CREATE_SUSPENDED,
    CREATE_UNICODE_ENVIRONMENT, INFINITE, PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, STARTF_USESTDHANDLES, STARTUPINFOW,
};
use windows_sys::Win32::System::IO::{
    CreateIoCompletionPort, GetQueuedCompletionStatus, OVERLAPPED,
};

use crate::cmdline::tokenize;
use crate::{parse_options, usage_error, WaitMode, EXIT_CANNOT_EXEC, EXIT_NOT_FOUND, EXIT_TIMEOUT};

/// How often we double-check job state in case a completion-port message is missed.
const POLL: Duration = Duration::from_millis(250);

fn die(context: &str, err: io::Error, code: i32) -> ! {
    eprintln!("jobexec: {context}: {err}");
    std::process::exit(code);
}

fn last_error() -> io::Error {
    io::Error::last_os_error()
}

/// Console Ctrl+C / Ctrl+Break are delivered to every process attached to the console, so the
/// child gets them directly. jobexec ignores them (a handler, not `SetConsoleCtrlHandler(NULL, TRUE)`,
/// because the latter would be inherited by the child). Close/logoff/shutdown fall through to the
/// default handler, which terminates jobexec and therefore the job.
unsafe extern "system" fn ctrl_handler(event: u32) -> i32 {
    if event == CTRL_C_EVENT || event == CTRL_BREAK_EVENT {
        TRUE
    } else {
        FALSE
    }
}

unsafe fn inheritable_dup(std: u32) -> HANDLE {
    let h = GetStdHandle(std);
    if h.is_null() || h == INVALID_HANDLE_VALUE {
        return null_mut();
    }
    let mut out: HANDLE = null_mut();
    let me = GetCurrentProcess();
    if DuplicateHandle(me, h, me, &mut out, 0, TRUE, DUPLICATE_SAME_ACCESS) == 0 {
        return null_mut();
    }
    out
}

fn filetime_u64(ft: &FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
}

unsafe fn creation_time(process: HANDLE) -> Option<u64> {
    let (mut c, mut e, mut k, mut u): (FILETIME, FILETIME, FILETIME, FILETIME) =
        (zeroed(), zeroed(), zeroed(), zeroed());
    (GetProcessTimes(process, &mut c, &mut e, &mut k, &mut u) != 0).then(|| filetime_u64(&c))
}

/// Opens a SYNCHRONIZE handle to our parent process, guarding against PID reuse by requiring the
/// parent to have been created before us.
unsafe fn open_parent() -> Option<HANDLE> {
    let me = GetCurrentProcessId();
    let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
    if snap == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut entry: PROCESSENTRY32W = zeroed();
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    let mut parent_pid = None;
    let mut ok = Process32FirstW(snap, &mut entry);
    while ok != 0 {
        if entry.th32ProcessID == me {
            parent_pid = Some(entry.th32ParentProcessID);
            break;
        }
        ok = Process32NextW(snap, &mut entry);
    }
    CloseHandle(snap);
    let parent = OpenProcess(
        PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
        FALSE,
        parent_pid?,
    );
    if parent.is_null() {
        return None;
    }
    match (creation_time(parent), creation_time(GetCurrentProcess())) {
        (Some(p), Some(m)) if p <= m => Some(parent),
        _ => {
            CloseHandle(parent);
            None
        }
    }
}

unsafe fn active_processes(job: HANDLE) -> Option<u32> {
    let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = zeroed();
    let ok = QueryInformationJobObject(
        job,
        JobObjectBasicAccountingInformation,
        &mut info as *mut _ as *mut _,
        size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
        null_mut(),
    );
    (ok != 0).then_some(info.ActiveProcesses)
}

pub fn main() {
    // Parse our own raw command line so the child's part can be forwarded byte-for-byte.
    let raw: Vec<u16> = unsafe {
        let p = GetCommandLineW();
        let mut n = 0;
        while *p.add(n) != 0 {
            n += 1;
        }
        std::slice::from_raw_parts(p, n).to_vec()
    };
    let tokens = tokenize(&raw);
    let args: Vec<String> = tokens
        .iter()
        .map(|t| String::from_utf16_lossy(&t.value))
        .collect();
    let opts = parse_options(&args);
    let program = &args[opts.command_index];
    if program.is_empty() {
        usage_error("empty command");
    }
    let mut child_cmdline: Vec<u16> = raw[tokens[opts.command_index].start..].to_vec();
    child_cmdline.push(0);

    unsafe { run(opts, program, child_cmdline) }
}

unsafe fn run(opts: crate::Options, program: &str, mut cmdline: Vec<u16>) -> ! {
    SetConsoleCtrlHandler(Some(ctrl_handler), TRUE);

    // 1. Job object: kill everything when the last handle (ours) closes; no WER dialogs.
    let job = CreateJobObjectW(null(), null());
    if job.is_null() {
        die("CreateJobObjectW", last_error(), EXIT_CANNOT_EXEC);
    }
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
    limits.BasicLimitInformation.LimitFlags =
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
    if SetInformationJobObject(
        job,
        JobObjectExtendedLimitInformation,
        &limits as *const _ as *const _,
        size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
    ) == 0
    {
        die(
            "SetInformationJobObject(limits)",
            last_error(),
            EXIT_CANNOT_EXEC,
        );
    }

    // 2. Completion port for "all processes in the job are gone" notifications.
    let port = CreateIoCompletionPort(INVALID_HANDLE_VALUE, null_mut(), 0, 1);
    if port.is_null() {
        die("CreateIoCompletionPort", last_error(), EXIT_CANNOT_EXEC);
    }
    let assoc = JOBOBJECT_ASSOCIATE_COMPLETION_PORT {
        CompletionKey: job,
        CompletionPort: port,
    };
    if SetInformationJobObject(
        job,
        JobObjectAssociateCompletionPortInformation,
        &assoc as *const _ as *const _,
        size_of::<JOBOBJECT_ASSOCIATE_COMPLETION_PORT>() as u32,
    ) == 0
    {
        die(
            "SetInformationJobObject(port)",
            last_error(),
            EXIT_CANNOT_EXEC,
        );
    }

    // 3. Start the child suspended with our stdio, put it in the job, then let it run. Because it
    //    is in the job before executing a single instruction, no descendant can escape.
    let mut si: STARTUPINFOW = zeroed();
    si.cb = size_of::<STARTUPINFOW>() as u32;
    si.dwFlags = STARTF_USESTDHANDLES;
    si.hStdInput = inheritable_dup(STD_INPUT_HANDLE);
    si.hStdOutput = inheritable_dup(STD_OUTPUT_HANDLE);
    si.hStdError = inheritable_dup(STD_ERROR_HANDLE);
    let mut pi: PROCESS_INFORMATION = zeroed();
    let created = CreateProcessW(
        null(),
        cmdline.as_mut_ptr(),
        null(),
        null(),
        TRUE,
        CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT,
        null(),
        null(),
        &si,
        &mut pi,
    );
    let create_err = last_error();
    for h in [si.hStdInput, si.hStdOutput, si.hStdError] {
        if !h.is_null() {
            CloseHandle(h);
        }
    }
    if created == 0 {
        let code = match create_err.raw_os_error().map(|e| e as u32) {
            Some(ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) => EXIT_NOT_FOUND,
            _ => EXIT_CANNOT_EXEC,
        };
        die(program, create_err, code);
    }
    if AssignProcessToJobObject(job, pi.hProcess) == 0 {
        let err = last_error();
        TerminateProcess(pi.hProcess, EXIT_CANNOT_EXEC as u32);
        die("AssignProcessToJobObject", err, EXIT_CANNOT_EXEC);
    }
    if ResumeThread(pi.hThread) == u32::MAX {
        let err = last_error();
        TerminateJobObject(job, EXIT_CANNOT_EXEC as u32);
        die("ResumeThread", err, EXIT_CANNOT_EXEC);
    }
    CloseHandle(pi.hThread);

    // 4. Optional: tie the tree's lifetime to our parent (e.g. the Node process that spawned us),
    //    covering the case where the parent dies without killing us.
    if opts.die_with_parent {
        if let Some(parent) = open_parent() {
            let (parent, job) = (parent as usize, job as usize);
            std::thread::spawn(move || {
                WaitForSingleObject(parent as HANDLE, INFINITE);
                TerminateJobObject(job as HANDLE, 1);
                std::process::exit(1);
            });
        }
    }

    // 5. Wait.
    let deadline = opts.timeout.map(|t| Instant::now() + t);
    let main_exited = || WaitForSingleObject(pi.hProcess, 0) == WAIT_OBJECT_0;
    loop {
        let done =
            main_exited() && (opts.wait == WaitMode::Main || active_processes(job) == Some(0));
        if done {
            break;
        }
        let mut slice = POLL;
        if let Some(d) = deadline {
            let left = d.saturating_duration_since(Instant::now());
            if left.is_zero() {
                TerminateJobObject(job, EXIT_TIMEOUT as u32);
                std::process::exit(EXIT_TIMEOUT);
            }
            slice = slice.min(left);
        }
        // With wait=main we only care about the child's handle; otherwise the job's port.
        if opts.wait == WaitMode::Main {
            WaitForSingleObject(pi.hProcess, slice.as_millis() as u32);
        } else {
            // Any job message (process exit, JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO, ...) just wakes us
            // up; the loop condition re-reads the authoritative state.
            let (mut msg, mut key, mut ov): (u32, usize, *mut OVERLAPPED) = (0, 0, null_mut());
            GetQueuedCompletionStatus(port, &mut msg, &mut key, &mut ov, slice.as_millis() as u32);
        }
    }

    let mut code: u32 = EXIT_CANNOT_EXEC as u32;
    GetExitCodeProcess(pi.hProcess, &mut code);
    // Kill leftovers explicitly (wait=main); with wait=all the job is already empty.
    TerminateJobObject(job, code);
    std::process::exit(code as i32);
}
