//! End-to-end tests: orphaned grandchildren must die with jobexec.
#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

const JOBEXEC: &str = env!("CARGO_BIN_EXE_jobexec");

fn alive(pid: u32) -> bool {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        let mut code = 0;
        let ok = GetExitCodeProcess(h, &mut code);
        CloseHandle(h);
        ok != 0 && code == STILL_ACTIVE as u32
    }
}

fn wait_dead(pid: u32, within: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < within {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn pid_file(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("jobexec-test-{}-{name}.pid", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

fn read_pid(p: &Path, within: Duration) -> u32 {
    let start = Instant::now();
    loop {
        if let Ok(s) = std::fs::read_to_string(p) {
            if let Ok(pid) = s.trim().parse() {
                return pid;
            }
        }
        assert!(
            start.elapsed() < within,
            "pid file {} never appeared",
            p.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// PowerShell command that starts an orphan `ping` (60s), records its pid, and exits with `code`.
fn orphan_maker(pid_path: &Path, code: i32) -> Vec<String> {
    let script = format!(
        "$p = Start-Process -FilePath ping.exe -ArgumentList '-n','60','127.0.0.1' -WindowStyle Hidden -PassThru; \
         Set-Content -Path '{}' -Value $p.Id; exit {code}",
        pid_path.display()
    );
    vec![
        "powershell.exe".into(),
        "-NoProfile".into(),
        "-NonInteractive".into(),
        "-Command".into(),
        script,
    ]
}

#[test]
fn exit_code_and_verbatim_args() {
    let out = Command::new(JOBEXEC)
        .args(["--", "cmd.exe", "/d", "/c", "echo [a  b] & exit 7"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(7));
    assert!(String::from_utf8_lossy(&out.stdout).contains("[a  b]"));
}

#[test]
fn not_found_is_127() {
    let out = Command::new(JOBEXEC)
        .args(["--", "definitely-not-a-real-program-xyz.exe"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(127));
}

#[test]
fn wait_main_kills_orphans_when_child_exits() {
    let pf = pid_file("main");
    let start = Instant::now();
    let status = Command::new(JOBEXEC)
        .arg("--wait=main")
        .arg("--")
        .args(orphan_maker(&pf, 3))
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(3));
    assert!(start.elapsed() < Duration::from_secs(30));
    let orphan = read_pid(&pf, Duration::from_secs(1));
    assert!(
        wait_dead(orphan, Duration::from_secs(5)),
        "orphan {orphan} survived"
    );
}

#[test]
fn killing_jobexec_kills_orphans() {
    let pf = pid_file("kill");
    let mut child = Command::new(JOBEXEC)
        .arg("--")
        .args(orphan_maker(&pf, 0))
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let orphan = read_pid(&pf, Duration::from_secs(30));
    // The direct child (powershell) exits right after writing the pid; wait=all keeps us alive.
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        child.try_wait().unwrap().is_none(),
        "jobexec exited while the orphan was alive"
    );
    assert!(alive(orphan));
    child.kill().unwrap(); // TerminateProcess, like Node's child.kill()
    child.wait().unwrap();
    assert!(
        wait_dead(orphan, Duration::from_secs(5)),
        "orphan {orphan} survived"
    );
}

#[test]
fn wait_all_returns_child_code_after_tree_finishes() {
    let start = Instant::now();
    let out = Command::new(JOBEXEC)
        .args([
            "--",
            "cmd.exe",
            "/d",
            "/c",
            "start /b ping -n 3 127.0.0.1 >nul & exit 5",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(5));
    // ping -n 3 takes ~2s; we must have waited for it.
    assert!(
        start.elapsed() >= Duration::from_millis(1500),
        "returned after {:?}",
        start.elapsed()
    );
}

#[test]
fn timeout_kills_tree() {
    let start = Instant::now();
    let status = Command::new(JOBEXEC)
        .args(["--timeout", "1", "--", "ping.exe", "-n", "60", "127.0.0.1"])
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(124));
    assert!(start.elapsed() < Duration::from_secs(10));
}
