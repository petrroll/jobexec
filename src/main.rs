//! jobexec: run a command so that its whole process tree can be killed as a unit.
//!
//! On Windows the child is started suspended, placed into a fresh Job Object with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, then resumed. Every descendant inherits the job,
//! even after its parent exits, so killing `jobexec` (e.g. Node's `child.kill()`, which is
//! `TerminateProcess`) closes the only job handle and the kernel kills the entire tree.
//!
//! Windows-only: on Unix, process groups already solve this (spawn with `setsid`, kill `-pgid`).

#[cfg(not(windows))]
compile_error!("jobexec is Windows-only; on Unix use process groups (setsid + kill(-pgid))");

mod cmdline;
mod windows;

use std::time::Duration;

/// Exit codes, aligned with GNU `timeout`/`env` where it makes sense.
pub const EXIT_TIMEOUT: i32 = 124;
pub const EXIT_USAGE: i32 = 125;
pub const EXIT_CANNOT_EXEC: i32 = 126;
pub const EXIT_NOT_FOUND: i32 = 127;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitMode {
    /// Exit once every process in the job has exited (like waiting for pipes to close).
    All,
    /// Exit as soon as the direct child exits; kill whatever it left behind.
    Main,
}

#[derive(Debug, Clone)]
pub struct Options {
    pub wait: WaitMode,
    pub timeout: Option<Duration>,
    pub die_with_parent: bool,
    /// Index of the first token that belongs to the child command.
    pub command_index: usize,
}

const HELP: &str = "\
jobexec - run a command so its whole process tree dies with it

USAGE:
    jobexec [OPTIONS] [--] <program> [args...]

The command runs inside a Windows Job Object with KILL_ON_JOB_CLOSE:
killing jobexec (TerminateProcess, taskkill /F, Node's child.kill()) kills
every descendant, even ones whose parent already exited. The child's command
line is forwarded verbatim. stdin/stdout/stderr are inherited.


OPTIONS:
    --wait <all|main>   all  (default): exit when every process in the job exited
                        main: exit when the direct child exits, killing leftovers
    --timeout <secs>    kill the whole tree after <secs> (fractional ok), exit 124
    --die-with-parent   kill the whole tree if jobexec's parent process exits
    -h, --help          print help
    -V, --version       print version

EXIT STATUS:
    the child's exit code, or 124 timeout, 125 usage error,
    126 command could not be started, 127 command not found
";

pub fn usage_error(msg: &str) -> ! {
    eprintln!("jobexec: {msg}");
    eprintln!("Try 'jobexec --help' for more information.");
    std::process::exit(EXIT_USAGE);
}

/// Parse jobexec's own options. `args[0]` is jobexec itself.
pub fn parse_options(args: &[String]) -> Options {
    let mut opts = Options {
        wait: WaitMode::All,
        timeout: None,
        die_with_parent: false,
        command_index: 0,
    };
    let mut i = 1;
    while i < args.len() {
        let a = args[i].as_str();
        let (name, inline) = match a.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n, Some(v.to_string())),
            _ => (a, None),
        };
        let value = |i: &mut usize| -> String {
            if let Some(v) = inline.clone() {
                return v;
            }
            *i += 1;
            args.get(*i)
                .cloned()
                .unwrap_or_else(|| usage_error(&format!("option '{name}' requires a value")))
        };
        match name {
            "--" => {
                i += 1;
                break;
            }
            "-h" | "--help" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("jobexec {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--wait" => {
                opts.wait = match value(&mut i).as_str() {
                    "all" => WaitMode::All,
                    "main" => WaitMode::Main,
                    other => usage_error(&format!(
                        "invalid --wait value '{other}' (expected all|main)"
                    )),
                }
            }
            "--timeout" => {
                let v = value(&mut i);
                let secs: f64 = v
                    .parse()
                    .unwrap_or_else(|_| usage_error(&format!("invalid --timeout '{v}'")));
                if !(secs.is_finite() && secs >= 0.0) {
                    usage_error(&format!("invalid --timeout '{v}'"));
                }
                opts.timeout = Some(Duration::from_secs_f64(secs));
            }
            "--die-with-parent" => opts.die_with_parent = true,
            _ if a.starts_with('-') && a.len() > 1 => usage_error(&format!("unknown option '{a}'")),
            _ => break,
        }
        i += 1;
    }
    if i >= args.len() {
        usage_error("missing command");
    }
    opts.command_index = i;
    opts
}

fn main() {
    windows::main();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(a: &[&str]) -> Options {
        parse_options(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn parses_options() {
        let o = p(&[
            "jobexec",
            "--wait=main",
            "--timeout",
            "1.5",
            "--die-with-parent",
            "--",
            "cmd",
            "/c",
        ]);
        assert_eq!(o.wait, WaitMode::Main);
        assert_eq!(o.timeout, Some(Duration::from_millis(1500)));
        assert!(o.die_with_parent);
        assert_eq!(o.command_index, 6);
    }

    #[test]
    fn command_without_separator() {
        let o = p(&["jobexec", "bash", "--wait=main"]);
        assert_eq!(o.wait, WaitMode::All);
        assert_eq!(o.command_index, 1);
    }
}
