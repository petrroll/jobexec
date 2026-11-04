# jobexec

Process-group semantics for Windows, as a tiny standalone binary.

```
jobexec [--wait all|main] [--timeout SECS] [--die-with-parent] -- <program> [args...]
```

`jobexec` starts `<program>` suspended, puts it in a new **Job Object** with
`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, and then resumes it. Every descendant inherits the job,
including ones whose parent has already exited, and none can break away. When `jobexec`
dies for any reason (`child.kill()`, `TerminateProcess`, `taskkill /F`, a crash), the kernel
closes the job handle and **kills the entire tree**.

On Linux and macOS it just `exec`s the command. Process groups already handle this there
(`spawn(..., { detached: true })` + `process.kill(-pid)`), so callers can keep one code path.

## Why

Starting a process tree and killing it later is a common pattern, e.g. a coding agent's
bash tool with a timeout or abort. On Windows the usual tools are `taskkill /T /F /PID`,
`tree-kill`, `@vscode/windows-process-tree` and PowerShell `Get-CimInstance`. All of them find
descendants **by parent PID**, which breaks when an intermediate process has already exited:

```bash
# bash exits after 1s with code 1; the background loop keeps the stdout pipe open forever
bash -c 'while true; do echo tick; sleep 0.05; done' & sleep 1; exit 1
```

Once the shell exits, the loop is an orphan. `taskkill /T /PID <shell>` is a no-op (exit 128),
the loop keeps writing to the inherited pipe, and the caller can't kill it or tell when it
ended (see [earendil-works/pi#11613](https://github.com/earendil-works/pi/issues/11613),
[#5303](https://github.com/earendil-works/pi/issues/5303)). The only reliable fix is a Job
Object, and Node has no API for that ([nodejs/node#64406](https://github.com/nodejs/node/issues/64406)).

Measured from Node on Windows 11 + Git Bash, running the command above and killing it at 3s:

| approach | caller sees exit | after kill |
|---|---|---|
| `spawn(bash)` + `taskkill /T /F` (status quo) | code 1 @ 1.1s | **loop leaked, still writing** |
| `spawn(jobexec -- bash ...)` + `child.kill()` | killed @ 3.0s | everything gone |
| `spawn(jobexec --wait=main -- bash ...)` | code 1 @ 1.1s, leftovers killed | everything gone |

### Existing options considered

- **Rust crates** [`process-wrap`](https://crates.io/crates/process-wrap) (formerly
  `command-group`), [`win32job`](https://crates.io/crates/win32job),
  [ProcessKit-rs](https://github.com/ZelAnton/ProcessKit-rs): all do Job Objects correctly, but
  they are libraries. A Node/Python/Go host can't use them without a native addon.
- **npm** `tree-kill`, `@a5omic/kill-tree`, `@vscode/windows-process-tree`: these walk the
  parent-PID tree, so orphans are missed.
- **[Process Governor](https://github.com/lowleveldesign/process-governor)** (`procgov`):
  a .NET resource limiter with a monitor process. Too heavy for this.

So jobexec is the missing piece: a roughly 250 KB exe with no runtime and no install step,
callable from any language.

## Options

| option | meaning |
|---|---|
| `--wait all` *(default)* | exit when **every** process in the job has exited, with the direct child's exit code. Matches "wait until the pipes close" semantics, so output from background descendants is not lost. |
| `--wait main` | exit as soon as the direct child exits, and kill whatever it left behind. Pipes close promptly and the caller gets the real exit code. |
| `--timeout SECS` | kill the whole tree after `SECS` (fractional allowed), exit `124` |
| `--die-with-parent` | kill the whole tree if jobexec's parent process exits, even if the parent never killed jobexec (e.g. Node crashed) |

Exit status: the child's exit code, or `124` timeout, `125` usage error, `126` cannot start,
`127` not found.

### Behaviour details

- **Verbatim command line.** Everything from `<program>` onward is passed to
  `CreateProcessW` byte-for-byte from jobexec's own command line, so quoting built by the
  caller (including Node's `windowsVerbatimArguments` for `cmd.exe /c`) reaches the child
  unchanged. Program lookup follows `CreateProcessW` rules, so pass a full path or an `.exe`
  on `PATH`. `.cmd`/`.bat` files need `cmd.exe /c`.
- **stdio** is inherited (stdin/stdout/stderr pipes, console or files).
- **Ctrl+C / Ctrl+Break** go to the child as usual. jobexec ignores them itself and stays
  until the tree is done. Console close, logoff and shutdown kill jobexec and therefore the tree.
- **Crash dialogs** are suppressed (`JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION`), so a
  crashing descendant can't hang the tree on a WER dialog.
- **Nested jobs** need Windows 8 / Server 2012 or newer, which covers every supported Windows.
  jobexec works when the caller is already in a job (CI runners, Windows Terminal, etc.).
- Breakaway is never allowed: `CREATE_BREAKAWAY_FROM_JOB` in a descendant fails.

## Use from Node.js

```ts
import { spawn } from "node:child_process";

const isWin = process.platform === "win32";
const child = isWin
  ? spawn(jobexecPath, ["--wait=main", "--", shell, ...args], { stdio: ["ignore", "pipe", "pipe"], windowsHide: true })
  : spawn(shell, args, { stdio: ["ignore", "pipe", "pipe"], detached: true });

function killTree() {
  if (isWin) child.kill();             // TerminateProcess(jobexec) -> job closed -> tree dead
  else process.kill(-child.pid!, "SIGKILL");
}
```

No `taskkill` is involved, and it works whether the direct child is still alive or not.

## Build

```
cargo build --release      # target/release/jobexec.exe
cargo test                 # Windows integration tests spawn real orphan trees
```

Prebuilt `x86_64` and `aarch64` Windows binaries are attached to
[GitHub releases](https://github.com/petrroll/jobexec/releases).

## License

MIT
