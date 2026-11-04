//! Unix: process groups already give tree-kill semantics (spawn detached, `kill(-pid)`), so
//! jobexec just replaces itself with the command. The pid the caller holds becomes the child's.

use std::os::unix::process::CommandExt;
use std::process::Command;

use crate::{parse_options, WaitMode, EXIT_CANNOT_EXEC, EXIT_NOT_FOUND};

pub fn main() {
    let args: Vec<String> = std::env::args().collect();
    let opts = parse_options(&args);
    // --wait/--timeout/--die-with-parent are Windows-only; they are accepted and ignored so
    // callers can pass the same arguments everywhere (documented in the README).
    let _ = (
        opts.wait == WaitMode::All,
        opts.timeout,
        opts.die_with_parent,
    );
    let err = Command::new(&args[opts.command_index])
        .args(&args[opts.command_index + 1..])
        .exec();
    eprintln!("jobexec: {}: {err}", args[opts.command_index]);
    std::process::exit(if err.kind() == std::io::ErrorKind::NotFound {
        EXIT_NOT_FOUND
    } else {
        EXIT_CANNOT_EXEC
    });
}
