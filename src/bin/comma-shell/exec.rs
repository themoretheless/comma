//! Execution: builtins, pipelines, redirects, `&&`/`||`/`;`, background jobs.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command as Proc, Stdio};

use crate::expand;
use crate::lexer::Part;
use crate::parser::{AndOr, Command, Connector, Pipeline, Redirect, Script};

/// State of a tracked job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Running,
    Stopped,
}

/// A background or stopped pipeline tracked by the shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub id: usize,
    /// Process group shared by all stages of the pipeline.
    pub pgid: i32,
    /// Stage pids not known to have exited yet.
    pub pids: Vec<i32>,
    pub command_line: String,
    pub state: JobState,
}

/// Mutable shell state shared by the REPL, builtins and the executor.
pub struct Shell {
    pub env: HashMap<String, String>,
    pub last_status: i32,
    pub should_exit: bool,
    pub history: Vec<String>,
    /// Background and stopped pipelines, oldest first.
    pub jobs: Vec<Job>,
    /// When `Some`, command stdout that would go to the terminal is appended
    /// here instead (command substitution capture).
    capture: Option<Vec<u8>>,
    /// The shell's own process group; the terminal is handed back to it
    /// after each foreground pipeline (job control).
    term_pgid: i32,
}

impl Shell {
    pub fn new() -> Self {
        Self::with_env(std::env::vars())
    }

    pub fn with_env<I, K, V>(vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        Self {
            env: vars.into_iter().map(|(k, v)| (k.into(), v.into())).collect(),
            last_status: 0,
            should_exit: false,
            history: Vec::new(),
            jobs: Vec::new(),
            capture: None,
            #[cfg(unix)]
            // SAFETY: getpgrp is always safe to call.
            term_pgid: unsafe { libc::getpgrp() },
            #[cfg(not(unix))]
            term_pgid: 0,
        }
    }

    /// Track a pipeline as a job; returns the assigned id (lowest free one).
    fn add_job(&mut self, pgid: i32, pids: Vec<i32>, command_line: String, state: JobState) -> usize {
        let id = (1..).find(|id| self.jobs.iter().all(|job| job.id != *id)).unwrap();
        self.jobs.push(Job { id, pgid, pids, command_line, state });
        id
    }

    /// Index of the job named by `spec` (`%n` or `n`); `None` picks the last.
    fn job_index(&self, spec: Option<&str>) -> Option<usize> {
        match spec {
            None => self.jobs.len().checked_sub(1),
            Some(spec) => {
                let id: usize = spec.strip_prefix('%').unwrap_or(spec).parse().ok()?;
                self.jobs.iter().position(|job| job.id == id)
            }
        }
    }

    /// Drop finished jobs and mark freshly stopped ones (WUNTRACED).
    /// A stopped job stays Stopped until `bg`/`fg` continues it: waitpid
    /// reports a stop transition only once, so "no event" must not
    /// downgrade the state back to Running.
    pub fn reap_jobs(&mut self) {
        #[cfg(unix)]
        {
            for job in &mut self.jobs {
                let mut stopped = false;
                job.pids.retain(|&pid| {
                    let mut status = 0;
                    // SAFETY: waitpid on our own child; `status` is valid.
                    let rv = unsafe {
                        libc::waitpid(pid, &mut status, libc::WNOHANG | libc::WUNTRACED)
                    };
                    if rv == pid {
                        if libc::WIFSTOPPED(status) {
                            stopped = true;
                            true // still alive, just stopped
                        } else {
                            false // exited or killed: drop
                        }
                    } else {
                        rv == 0 // 0: no news; -1 (ECHILD): already gone
                    }
                });
                if stopped {
                    job.state = JobState::Stopped;
                }
            }
            self.jobs.retain(|job| !job.pids.is_empty());
        }
    }

    /// `fg [%n]`: continue the job and wait for it in the foreground.
    #[cfg(unix)]
    pub(crate) fn foreground(&mut self, spec: Option<&str>, out: &mut dyn Write) -> i32 {
        self.reap_jobs();
        let Some(index) = self.job_index(spec) else {
            let _ = writeln!(out, "comma-shell: fg: no such job");
            return 1;
        };
        let job = &self.jobs[index];
        let pgid = job.pgid;
        let pids = job.pids.clone();
        let label = job.command_line.clone();

        // SAFETY: signaling a process group of our own children.
        unsafe {
            libc::kill(-pgid, libc::SIGCONT);
        }
        self.jobs[index].state = JobState::Running;
        terminal_to(pgid);
        // Echo the command line like bash does. Printed directly (not via
        // `out`, which flushes only after the wait) and only after the
        // terminal handoff, so the message means "the job is foreground now".
        println!("{label}");
        let _ = std::io::stdout().flush();

        let mut status = 0;
        let mut stopped = false;
        let mut remaining = Vec::new();
        for (i, pid) in pids.iter().enumerate() {
            let mut raw = 0;
            // SAFETY: waitpid on our own child; `raw` is a valid out-pointer.
            let rv = unsafe {
                loop {
                    let rv = libc::waitpid(*pid, &mut raw, libc::WUNTRACED);
                    let interrupted = rv == -1
                        && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR);
                    if !interrupted {
                        break rv;
                    }
                }
            };
            if rv == *pid {
                if libc::WIFSTOPPED(raw) {
                    stopped = true;
                    status = 128 + libc::WSTOPSIG(raw);
                    remaining.extend_from_slice(&pids[i..]);
                    break;
                }
                status = exit_code_from_raw(raw);
            }
            // rv == -1 (ECHILD): already reaped elsewhere; drop it.
        }
        terminal_to(self.term_pgid);

        if stopped {
            let job = &mut self.jobs[index];
            job.state = JobState::Stopped;
            job.pids = remaining;
            let _ = writeln!(out, "[{}]+ Stopped  {label}", job.id);
        } else {
            self.jobs.remove(index);
        }
        status
    }

    /// `bg [%n]`: continue a stopped job in the background.
    #[cfg(unix)]
    pub(crate) fn background(&mut self, spec: Option<&str>, out: &mut dyn Write) -> i32 {
        self.reap_jobs();
        let Some(index) = self.job_index(spec) else {
            let _ = writeln!(out, "comma-shell: bg: no such job");
            return 1;
        };
        let job = &mut self.jobs[index];
        // SAFETY: signaling a process group of our own children.
        unsafe {
            libc::kill(-job.pgid, libc::SIGCONT);
        }
        job.state = JobState::Running;
        let _ = writeln!(out, "[{}]  {}", job.id, job.command_line);
        0
    }

    #[cfg(not(unix))]
    pub(crate) fn foreground(&mut self, _spec: Option<&str>, out: &mut dyn Write) -> i32 {
        let _ = writeln!(out, "comma-shell: fg: job control is not supported here");
        1
    }

    #[cfg(not(unix))]
    pub(crate) fn background(&mut self, _spec: Option<&str>, out: &mut dyn Write) -> i32 {
        let _ = writeln!(out, "comma-shell: bg: job control is not supported here");
        1
    }
}

pub fn execute_script(shell: &mut Shell, script: &Script) -> i32 {
    let mut status = 0;
    for and_or in &script.seq {
        status = execute_and_or(shell, and_or);
        if shell.should_exit {
            break;
        }
    }
    shell.last_status = status;
    status
}

fn execute_and_or(shell: &mut Shell, and_or: &AndOr) -> i32 {
    let mut status = execute_pipeline(shell, &and_or.first);
    for (connector, pipeline) in &and_or.rest {
        let skip = match connector {
            Connector::And => status != 0,
            Connector::Or => status == 0,
        };
        if !skip {
            status = execute_pipeline(shell, pipeline);
        }
        if shell.should_exit {
            break;
        }
    }
    status
}

/// Where a command's stdout should go.
enum StdoutTarget {
    /// Inherit the shell's stdout.
    Inherit,
    /// Capture into a buffer (feeds the next pipeline stage).
    Capture,
    /// Write to a file.
    File { path: PathBuf, append: bool },
}

/// Result of one pipeline stage: exit status and captured stdout.
struct CmdOutcome {
    status: i32,
    captured: Option<Vec<u8>>,
}

fn execute_pipeline(shell: &mut Shell, pipeline: &Pipeline) -> i32 {
    debug_assert!(!pipeline.cmds.is_empty(), "parser never yields empty pipelines");

    // Resolve $(...) substitutions before anything inspects argv.
    let pipeline = &substitute_pipeline(shell, pipeline);

    if pipeline.background {
        return execute_background(shell, pipeline);
    }

    // All-external pipelines stream: every stage spawns at once, connected by
    // pipes, instead of buffering each stage's whole output in memory.
    if pipeline.cmds.len() > 1 && pipeline.cmds.iter().all(|cmd| is_external(shell, cmd)) {
        return execute_streaming(shell, pipeline);
    }

    let mut stdin_data: Option<Vec<u8>> = None;
    let mut status = 0;
    let last_index = pipeline.cmds.len() - 1;

    for (index, cmd) in pipeline.cmds.iter().enumerate() {
        let argv = expand::expand_argv(&cmd.argv, &shell.env, shell.last_status);
        let last = index == last_index;

        let redirects = resolve_redirects(shell, cmd, last);
        let out_target = redirects.out;
        let err_file = redirects.err;
        let in_file = redirects.input;

        // Bare redirects (no command): just create/truncate the files.
        if argv.is_empty() {
            let mut ok = true;
            if let StdoutTarget::File { path, append } = &out_target {
                ok &= touch(path, *append);
            }
            if let Some((path, append)) = &err_file {
                ok &= touch(path, *append);
            }
            status = if ok { 0 } else { 1 };
            continue;
        }

        // Builtins write into a buffer so they compose with pipes/redirects.
        let mut out = Vec::new();
        match crate::builtins::run(shell, &argv, &mut out) {
            Some(code) => {
                status = code;
                match out_target {
                    StdoutTarget::Inherit => write_out(shell, &out),
                    StdoutTarget::Capture => stdin_data = Some(out),
                    StdoutTarget::File { path, append } => match open_file(&path, append) {
                        Ok(mut file) => {
                            let _ = file.write_all(&out);
                        }
                        Err(err) => {
                            eprintln!("comma-shell: {}: {err}", path.display());
                            status = 1;
                        }
                    },
                }
            }
            None => {
                match run_external(shell, &argv, &out_target, &err_file, &in_file, stdin_data.take())
                {
                    Ok(outcome) => {
                        status = outcome.status;
                        if last {
                            // Under substitution capture the last stage's
                            // stdout was piped instead of inherited.
                            if let Some(bytes) = outcome.captured {
                                write_out(shell, &bytes);
                            }
                        } else {
                            stdin_data = outcome.captured;
                        }
                    }
                    Err(code) => {
                        status = code;
                        break;
                    }
                }
            }
        }

        if shell.should_exit {
            break;
        }
    }

    status
}

/// Whether the command's (expanded) name is not a builtin. Bare redirects
/// (empty argv) don't count.
fn is_external(shell: &Shell, cmd: &Command) -> bool {
    let name = expand::expand_argv(&cmd.argv[..1.min(cmd.argv.len())], &shell.env, shell.last_status);
    name.first().is_some_and(|name| !crate::builtins::is_builtin(name))
}

/// Display label of a pipeline for the job table: expanded argv words
/// joined by spaces, stages by ` | `.
fn pipeline_label(shell: &Shell, pipeline: &Pipeline) -> String {
    pipeline
        .cmds
        .iter()
        .map(|cmd| expand::expand_argv(&cmd.argv, &shell.env, shell.last_status).join(" "))
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Write command output that targets the terminal: into the capture buffer
/// during command substitution, straight to stdout otherwise.
fn write_out(shell: &mut Shell, bytes: &[u8]) {
    match &mut shell.capture {
        Some(buf) => buf.extend_from_slice(bytes),
        None => {
            let _ = std::io::stdout().write_all(bytes);
        }
    }
}

/// Clone `pipeline` with every `Part::Subst` replaced by the captured output
/// of its command line (as a quoted literal: no re-expansion, no globbing,
/// no word splitting).
fn substitute_pipeline(shell: &mut Shell, pipeline: &Pipeline) -> Pipeline {
    let mut pipeline = pipeline.clone();
    for cmd in &mut pipeline.cmds {
        for word in &mut cmd.argv {
            substitute_word(shell, word);
        }
        for redirect in &mut cmd.redirects {
            let target = match redirect {
                Redirect::In(target)
                | Redirect::Out { target, .. }
                | Redirect::ErrOut { target, .. } => target,
            };
            substitute_word(shell, target);
        }
    }
    pipeline
}

fn substitute_word(shell: &mut Shell, word: &mut Vec<Part>) {
    for part in word.iter_mut() {
        if let Part::Subst(line) = part {
            let output = run_substitution(shell, line);
            *part = Part::QLit(output);
        }
    }
}

/// Execute a `$(...)` body as a sub-shell script and capture its stdout;
/// trailing newlines are stripped (bash behavior). Parse errors and failing
/// commands yield (possibly partial) output and never propagate: the outer
/// command's status is unaffected. `exit` inside a substitution does not
/// leave the sub-shell.
fn run_substitution(shell: &mut Shell, line: &str) -> String {
    let Ok(script) = crate::parser::parse(line) else {
        eprintln!("comma-shell: $({line}): parse error");
        return String::new();
    };
    let saved_capture = shell.capture.replace(Vec::new());
    let saved_exit = shell.should_exit;
    shell.should_exit = false;
    execute_script(shell, &script);
    let out = shell.capture.take().unwrap_or_default();
    shell.capture = saved_capture;
    shell.should_exit = saved_exit;
    let mut text = String::from_utf8_lossy(&out).into_owned();
    while text.ends_with('\n') {
        text.pop();
    }
    text
}

/// `cmd &`: spawn the pipeline detached — no terminal handoff, no waiting;
/// the job is reaped later by `Shell::reap_jobs`.
fn execute_background(shell: &mut Shell, pipeline: &Pipeline) -> i32 {
    // Bare redirects (`> file &`): nothing to background, run as foreground.
    if pipeline.cmds.iter().all(|cmd| cmd.argv.is_empty()) {
        let mut foreground = pipeline.clone();
        foreground.background = false;
        return execute_pipeline(shell, &foreground);
    }
    // Builtins run in-process and can't be detached (no subshell support).
    if !pipeline.cmds.iter().all(|cmd| is_external(shell, cmd)) {
        eprintln!("comma-shell: '&' is only supported for external commands");
        return 1;
    }
    match spawn_stages(shell, pipeline, false) {
        Ok(spawned) => {
            let label = pipeline_label(shell, pipeline);
            let id = shell.add_job(spawned.pgid, spawned.pids, label, JobState::Running);
            println!("[{id}] {}", spawned.pgid);
            0
        }
        Err(code) => code,
    }
}

fn touch(path: &Path, append: bool) -> bool {
    match open_file(path, append) {
        Ok(_) => true,
        Err(err) => {
            eprintln!("comma-shell: {}: {err}", path.display());
            false
        }
    }
}

fn open_file(path: &Path, append: bool) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(!append)
        .append(append)
        .open(path)
}

/// Redirects of one pipeline stage after word expansion.
struct StageRedirects {
    out: StdoutTarget,
    err: Option<(PathBuf, bool)>,
    input: Option<PathBuf>,
}

fn resolve_redirects(shell: &Shell, cmd: &Command, last: bool) -> StageRedirects {
    let mut redirects = StageRedirects {
        out: if last { StdoutTarget::Inherit } else { StdoutTarget::Capture },
        err: None,
        input: None,
    };
    for redirect in &cmd.redirects {
        match redirect {
            Redirect::In(target) => {
                redirects.input =
                    Some(expand::expand_word(target, &shell.env, shell.last_status).into());
            }
            Redirect::Out { target, append } => {
                redirects.out = StdoutTarget::File {
                    path: expand::expand_word(target, &shell.env, shell.last_status).into(),
                    append: *append,
                };
            }
            Redirect::ErrOut { target, append } => {
                redirects.err = Some((
                    expand::expand_word(target, &shell.env, shell.last_status).into(),
                    *append,
                ));
            }
        }
    }
    redirects
}

/// Prepare an external child: put it into the pipeline's process group and
/// restore default signal handlers.
///
/// Invariants: the shell itself ignores SIGINT/SIGQUIT/SIGTSTP/SIGTTOU/SIGTTIN
/// (see main.rs), so children must reset them to SIG_DFL, and every external
/// pipeline gets its own process group so terminal signals (Ctrl+C/Ctrl+Z)
/// reach only the pipeline, not the shell. With SIGTTOU/SIGTTIN back at
/// SIG_DFL, a background job that writes to (or reads from) the terminal is
/// stopped by the kernel — standard unix job-control behavior. `pgid` is 0
/// for the pipeline leader (a new group with the child's own pid) or the
/// leader's pid for the rest. Both the child (here) and the parent
/// (`assign_group`) call setpgid because the parent/child execution order
/// is a race.
fn prepare_child(command: &mut Proc, pgid: i32) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: pre_exec runs in the child between fork and exec;
        // setpgid/signal are async-signal-safe.
        unsafe {
            command.pre_exec(move || {
                libc::setpgid(0, pgid);
                for sig in
                    [libc::SIGINT, libc::SIGQUIT, libc::SIGTSTP, libc::SIGTTOU, libc::SIGTTIN]
                {
                    libc::signal(sig, libc::SIG_DFL);
                }
                Ok(())
            });
        }
    }
    #[cfg(not(unix))]
    let _ = pgid;
}

/// Parent-side setpgid after spawn; may fail with EACCES if the child already
/// exec'd (it did its own setpgid in pre_exec) — that race is expected.
#[cfg(unix)]
fn assign_group(pid: u32, pgid: i32) {
    // SAFETY: setpgid on our own child; errors are intentionally ignored.
    unsafe {
        libc::setpgid(pid as i32, pgid);
    }
}

/// Hand the terminal (stdin fd) to a process group. Best-effort: fails
/// harmlessly when stdin is not a tty (tests, pipes). The shell ignores
/// SIGTTOU/SIGTTIN, so this never stops the shell itself.
#[cfg(unix)]
fn terminal_to(pgid: i32) {
    // SAFETY: tcsetpgrp with a valid fd and process group; errors ignored.
    unsafe {
        libc::tcsetpgrp(0, pgid);
    }
}

/// Outcome of waiting for one child.
struct WaitOutcome {
    status: i32,
    /// The child was stopped (Ctrl+Z) and left stopped.
    stopped: bool,
    captured: Option<Vec<u8>>,
}

#[cfg(unix)]
fn exit_code_from_raw(status: i32) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        1
    }
}

/// Wait for a child, noticing stops (WUNTRACED): Ctrl+Z must not hang the
/// shell — the stopped child is simply left behind. `name` is used for
/// error messages only.
fn wait_child(mut child: std::process::Child, capture_stdout: bool, name: &str) -> WaitOutcome {
    #[cfg(unix)]
    {
        // Drain stdout from a thread so a chatty child can't block on a full
        // pipe while we waitpid it.
        let reader = if capture_stdout {
            child.stdout.take().map(|mut out| {
                std::thread::spawn(move || {
                    let mut buf = Vec::new();
                    let _ = std::io::Read::read_to_end(&mut out, &mut buf);
                    buf
                })
            })
        } else {
            None
        };

        let pid = child.id() as i32;
        let mut status = 0;
        // SAFETY: waitpid on our own child; `status` is a valid out-pointer.
        let rv = unsafe {
            loop {
                let rv = libc::waitpid(pid, &mut status, libc::WUNTRACED);
                let interrupted = rv == -1
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR);
                if !interrupted {
                    break rv;
                }
            }
        };
        if rv == pid {
            if libc::WIFSTOPPED(status) {
                // Stopped by Ctrl+Z: leave it stopped; don't join the stdout
                // reader, the pipe stays open while the child is stopped.
                return WaitOutcome { status: 128 + libc::SIGTSTP, stopped: true, captured: None };
            }
            let captured = reader.map(|thread| thread.join().unwrap_or_default());
            return WaitOutcome {
                status: exit_code_from_raw(status),
                stopped: false,
                captured,
            };
        }
        // waitpid failed: fall through to std wait below.
    }

    let output = if capture_stdout {
        child.wait_with_output()
    } else {
        child.wait().map(|status| std::process::Output {
            status,
            stdout: Vec::new(),
            stderr: Vec::new(),
        })
    };
    match output {
        Ok(output) => WaitOutcome {
            status: exit_code(&output.status),
            stopped: false,
            captured: if capture_stdout { Some(output.stdout) } else { None },
        },
        Err(err) => {
            eprintln!("comma-shell: {name}: {err}");
            WaitOutcome { status: 1, stopped: false, captured: None }
        }
    }
}

/// A spawned all-external pipeline: child handles, their pids and the shared
/// process group (pid of the first stage).
struct Spawned {
    children: Vec<std::process::Child>,
    pids: Vec<i32>,
    pgid: i32,
    /// Stdout of the last stage when capturing (command substitution).
    final_stdout: Option<std::process::ChildStdout>,
}

/// Spawn every stage of an all-external pipeline at once, wiring each stage's
/// stdout into the next stage's stdin, so output streams through pipes
/// without buffering whole stages in memory. With `capture_last` the last
/// stage's stdout is piped into `Spawned::final_stdout` instead of the
/// terminal. On failure the already-spawned stages are killed and the error
/// code returned.
fn spawn_stages(shell: &Shell, pipeline: &Pipeline, capture_last: bool) -> Result<Spawned, i32> {
    let last_index = pipeline.cmds.len() - 1;
    let mut children = Vec::new();
    let mut prev_stdout: Option<std::process::ChildStdout> = None;
    let mut final_stdout: Option<std::process::ChildStdout> = None;
    let mut failed: Option<i32> = None;
    let mut group = 0; // pipeline process group; pid of the first stage

    for (index, cmd) in pipeline.cmds.iter().enumerate() {
        let argv = expand::expand_argv(&cmd.argv, &shell.env, shell.last_status);
        let redirects = resolve_redirects(shell, cmd, index == last_index);

        let mut command = Proc::new(&argv[0]);
        command.args(&argv[1..]).env_clear().envs(&shell.env);
        prepare_child(&mut command, group);

        // Stdin: redirected file, previous stage's stdout, or the terminal.
        let stdin = match &redirects.input {
            Some(path) => match std::fs::File::open(path) {
                Ok(file) => Stdio::from(file),
                Err(err) => {
                    eprintln!("comma-shell: {}: {err}", path.display());
                    failed = Some(1);
                    break;
                }
            },
            None => match prev_stdout.take() {
                Some(stdout) => Stdio::from(stdout),
                None => Stdio::inherit(),
            },
        };
        command.stdin(stdin);

        // Stdout: file, pipe to the next stage, capture buffer, or terminal.
        let mut piped = false;
        let mut piped_final = false;
        match &redirects.out {
            StdoutTarget::Inherit => {
                if capture_last && index == last_index {
                    command.stdout(Stdio::piped());
                    piped_final = true;
                } else {
                    command.stdout(Stdio::inherit());
                }
            }
            StdoutTarget::Capture => {
                command.stdout(Stdio::piped());
                piped = true;
            }
            StdoutTarget::File { path, append } => match open_file(path, *append) {
                Ok(file) => {
                    command.stdout(Stdio::from(file));
                }
                Err(err) => {
                    eprintln!("comma-shell: {}: {err}", path.display());
                    failed = Some(1);
                    break;
                }
            },
        }

        if let Some((path, append)) = &redirects.err {
            match open_file(path, *append) {
                Ok(file) => {
                    command.stderr(Stdio::from(file));
                }
                Err(err) => {
                    eprintln!("comma-shell: {}: {err}", path.display());
                    failed = Some(1);
                    break;
                }
            }
        }

        match command.spawn() {
            Ok(mut child) => {
                let pid = child.id();
                if group == 0 {
                    group = pid as i32;
                }
                assign_group(pid, group);
                if piped {
                    prev_stdout = child.stdout.take();
                } else if piped_final {
                    final_stdout = child.stdout.take();
                }
                children.push(child);
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("comma-shell: command not found: {}", argv[0]);
                failed = Some(127);
                break;
            }
            Err(err) => {
                eprintln!("comma-shell: {}: {err}", argv[0]);
                failed = Some(1);
                break;
            }
        }
    }

    if failed.is_some() {
        // A stage never started: stop the ones already running.
        for mut child in children {
            let _ = child.kill();
            let _ = child.wait();
        }
        return Err(failed.unwrap_or(1));
    }

    let pids = children.iter().map(|child| child.id() as i32).collect();
    Ok(Spawned { children, pids, pgid: group, final_stdout })
}

/// Foreground all-external pipeline: all stages share one process group and
/// get the terminal while running, so Ctrl+C/Ctrl+Z apply to the whole
/// pipeline. A stopped pipeline (Ctrl+Z) is tracked as a job for `fg`/`bg`.
fn execute_streaming(shell: &mut Shell, pipeline: &Pipeline) -> i32 {
    let spawned = match spawn_stages(shell, pipeline, shell.capture.is_some()) {
        Ok(spawned) => spawned,
        Err(code) => return code,
    };
    let label = pipeline_label(shell, pipeline);
    let last_index = spawned.children.len() - 1;
    // Drain the captured last-stage stdout from a thread so a chatty stage
    // can't block on a full pipe while we wait for the pipeline.
    let drain = spawned.final_stdout.map(|mut out| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = std::io::Read::read_to_end(&mut out, &mut buf);
            buf
        })
    });
    #[cfg(unix)]
    terminal_to(spawned.pgid);
    let mut status = 0;
    let mut stopped_at = None;
    for (index, child) in spawned.children.into_iter().enumerate() {
        let outcome = wait_child(child, false, "pipeline");
        if index == last_index || outcome.stopped {
            status = outcome.status;
        }
        if outcome.stopped {
            stopped_at = Some(index);
            break;
        }
    }
    #[cfg(unix)]
    terminal_to(shell.term_pgid);
    if let Some(drain) = drain {
        let bytes = drain.join().unwrap_or_default();
        write_out(shell, &bytes);
    }
    if let Some(index) = stopped_at {
        let pids = spawned.pids[index..].to_vec();
        let id = shell.add_job(spawned.pgid, pids, label.clone(), JobState::Stopped);
        println!("[{id}]+ Stopped  {label}");
    }
    status
}

fn run_external(
    shell: &mut Shell,
    argv: &[String],
    out_target: &StdoutTarget,
    err_file: &Option<(PathBuf, bool)>,
    in_file: &Option<PathBuf>,
    stdin_data: Option<Vec<u8>>,
) -> Result<CmdOutcome, i32> {
    let mut command = Proc::new(&argv[0]);
    command.args(&argv[1..]).env_clear().envs(&shell.env);

    // Stdin: redirected file, previous stage output, or the terminal.
    let input = match in_file {
        Some(path) => match std::fs::read(path) {
            Ok(bytes) => Some(bytes),
            Err(err) => {
                eprintln!("comma-shell: {}: {err}", path.display());
                return Err(1);
            }
        },
        None => stdin_data,
    };
    command.stdin(if input.is_some() { Stdio::piped() } else { Stdio::inherit() });

    // Stdout: file, pipe to the next stage, or the terminal.
    //
    // Known limitation: this buffered path runs one stage at a time with the
    // whole stdout captured in memory; it is only used for pipelines
    // containing a builtin (all-external pipelines stream, see
    // `execute_streaming`).
    let capture_stdout = match out_target {
        StdoutTarget::Inherit => {
            // During command substitution "inherit" means "capture".
            if shell.capture.is_some() {
                command.stdout(Stdio::piped());
                true
            } else {
                command.stdout(Stdio::inherit());
                false
            }
        }
        StdoutTarget::Capture => {
            command.stdout(Stdio::piped());
            true
        }
        StdoutTarget::File { path, append } => match open_file(path, *append) {
            Ok(file) => {
                command.stdout(Stdio::from(file));
                false
            }
            Err(err) => {
                eprintln!("comma-shell: {}: {err}", path.display());
                return Err(1);
            }
        },
    };

    if let Some((path, append)) = err_file {
        match open_file(path, *append) {
            Ok(file) => {
                command.stderr(Stdio::from(file));
            }
            Err(err) => {
                eprintln!("comma-shell: {}: {err}", path.display());
                return Err(1);
            }
        }
    }

    // The command runs in its own process group (pgid = own pid).
    prepare_child(&mut command, 0);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("comma-shell: command not found: {}", argv[0]);
            return Err(127);
        }
        Err(err) => {
            eprintln!("comma-shell: {}: {err}", argv[0]);
            return Err(1);
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        let data = input.unwrap_or_default();
        // Write from a thread so a large input can't deadlock against a
        // child that writes more than the pipe buffer holds.
        std::thread::spawn(move || {
            let _ = stdin.write_all(&data);
        });
    }

    #[cfg(unix)]
    let pgid = child.id() as i32;
    #[cfg(unix)]
    {
        assign_group(child.id(), pgid);
        // Foreground the command's group, then take the terminal back.
        terminal_to(pgid);
    }
    let outcome = wait_child(child, capture_stdout, &argv[0]);
    #[cfg(unix)]
    terminal_to(shell.term_pgid);
    #[cfg(unix)]
    if outcome.stopped {
        // Ctrl+Z: track the stopped command so `fg`/`bg` can resume it.
        let label = argv.join(" ");
        let id = shell.add_job(pgid, vec![pgid], label.clone(), JobState::Stopped);
        println!("[{id}]+ Stopped  {label}");
    }
    Ok(CmdOutcome { status: outcome.status, captured: outcome.captured })
}

fn exit_code(status: &std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return 128 + sig;
        }
    }
    status.code().unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;

    /// Run a command line and return its exit status.
    fn run(shell: &mut Shell, line: &str) -> i32 {
        let script = parser::parse(line).unwrap();
        execute_script(shell, &script)
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "comma-shell-test-{}-{}",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn redirect_out_and_append() {
        let mut shell = Shell::with_env(Vec::<(String, String)>::new());
        let dir = temp_dir("redirect");
        let out = dir.join("out.txt");
        assert_eq!(run(&mut shell, &format!("echo hello > {}", out.display())), 0);
        assert_eq!(run(&mut shell, &format!("echo again >> {}", out.display())), 0);
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "hello\nagain\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pipeline_feeds_next_command() {
        let mut shell = Shell::with_env(Vec::<(String, String)>::new());
        let dir = temp_dir("pipeline");
        let out = dir.join("out.txt");
        let line = format!("echo hello | cat | cat > {}", out.display());
        assert_eq!(run(&mut shell, &line), 0);
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "hello\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn redirect_in() {
        let mut shell = Shell::with_env(Vec::<(String, String)>::new());
        let dir = temp_dir("redirect-in");
        let input = dir.join("in.txt");
        let out = dir.join("out.txt");
        std::fs::write(&input, "data\n").unwrap();
        let line = format!("cat < {} > {}", input.display(), out.display());
        assert_eq!(run(&mut shell, &line), 0);
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "data\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn and_or_uses_exit_status() {
        let mut shell = Shell::with_env(Vec::<(String, String)>::new());
        let dir = temp_dir("and-or");
        let yes = dir.join("yes");
        let no = dir.join("no");
        let line = format!("true && echo ok > {}; false && echo bad > {}", yes.display(), no.display());
        assert_eq!(run(&mut shell, &line), 1);
        assert!(yes.exists());
        assert!(!no.exists());
        let line = format!("false || echo fb > {}", no.display());
        assert_eq!(run(&mut shell, &line), 0);
        assert_eq!(std::fs::read_to_string(&no).unwrap(), "fb\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unknown_command_is_127() {
        let mut shell = Shell::with_env(Vec::<(String, String)>::new());
        assert_eq!(run(&mut shell, "definitely-not-a-command-xyz"), 127);
        assert_eq!(shell.last_status, 127);
    }

    #[test]
    fn expansions_apply() {
        let mut shell = Shell::with_env([("FOO".to_string(), "bar".to_string())]);
        let dir = temp_dir("expand");
        let out = dir.join("out.txt");
        assert_eq!(run(&mut shell, &format!("echo $FOO-x > {}", out.display())), 0);
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "bar-x\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn external_pipeline_streams() {
        // All stages external (cat): the streaming path runs them together.
        let mut shell = Shell::with_env(Vec::<(String, String)>::new());
        let dir = temp_dir("stream");
        let input = dir.join("in.txt");
        let out = dir.join("out.txt");
        std::fs::write(&input, "data\n").unwrap();
        let line = format!("cat {} | cat | cat > {}", input.display(), out.display());
        assert_eq!(run(&mut shell, &line), 0);
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "data\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn early_exit_of_sink_terminates_pipeline() {
        // `yes` never ends on its own; only a streaming pipeline where head's
        // exit kills the writer via SIGPIPE terminates here. A buffered
        // implementation would hang this test forever.
        let mut shell = Shell::with_env(Vec::<(String, String)>::new());
        let dir = temp_dir("yes-head");
        let out = dir.join("out.txt");
        let line = format!("yes | head -n 3 > {}", out.display());
        assert_eq!(run(&mut shell, &line), 0);
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "y\ny\ny\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unknown_command_in_streaming_pipeline() {
        let mut shell = Shell::with_env(Vec::<(String, String)>::new());
        assert_eq!(run(&mut shell, "cat /dev/null | definitely-not-a-command-xyz"), 127);
    }

    #[test]
    fn job_ids_reuse_freed_slots() {
        let mut shell = Shell::with_env(Vec::<(String, String)>::new());
        assert_eq!(shell.add_job(101, vec![101], "a".into(), JobState::Running), 1);
        assert_eq!(shell.add_job(102, vec![102], "b".into(), JobState::Stopped), 2);
        shell.jobs.retain(|job| job.id != 1); // job 1 finished
        assert_eq!(shell.add_job(103, vec![103], "c".into(), JobState::Running), 1);
    }

    #[test]
    fn job_index_parses_specs() {
        let mut shell = Shell::with_env(Vec::<(String, String)>::new());
        shell.add_job(201, vec![201], "a".into(), JobState::Running);
        shell.add_job(202, vec![202], "b".into(), JobState::Running);
        assert_eq!(shell.job_index(None), Some(1)); // last job
        assert_eq!(shell.job_index(Some("%1")), Some(0));
        assert_eq!(shell.job_index(Some("2")), Some(1));
        assert_eq!(shell.job_index(Some("%9")), None);
        assert_eq!(shell.job_index(Some("junk")), None);
    }

    #[cfg(unix)]
    #[test]
    fn reap_drops_jobs_whose_processes_are_gone() {
        let mut shell = Shell::with_env(Vec::<(String, String)>::new());
        // No such child: waitpid reports ECHILD and the job is dropped.
        shell.add_job(i32::MAX - 1, vec![i32::MAX - 1], "gone".into(), JobState::Running);
        shell.reap_jobs();
        assert!(shell.jobs.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn background_command_registers_a_running_job() {
        let mut shell = Shell::with_env(Vec::<(String, String)>::new());
        assert_eq!(run(&mut shell, "sleep 61 &"), 0);
        assert_eq!(shell.jobs.len(), 1);
        assert_eq!(shell.jobs[0].state, JobState::Running);
        assert_eq!(shell.jobs[0].command_line, "sleep 61");

        // Kill the job's group; reaping then drops it.
        let pgid = shell.jobs[0].pgid;
        // SAFETY: signaling a process group of our own child.
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !shell.jobs.is_empty() {
            shell.reap_jobs();
            assert!(std::time::Instant::now() < deadline, "killed job was not reaped");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn background_builtin_is_rejected() {
        // Builtins run in-process and can't be detached.
        let mut shell = Shell::with_env(Vec::<(String, String)>::new());
        assert_eq!(run(&mut shell, "echo hi &"), 1);
        assert!(shell.jobs.is_empty());
    }

    /// Run a substitution command line and return the substituted argv.
    fn subst(shell: &mut Shell, line: &str) -> Vec<String> {
        let script = crate::parser::parse(line).unwrap();
        let pipeline = substitute_pipeline(shell, &script.seq[0].first);
        pipeline.cmds[0]
            .argv
            .iter()
            .map(|word| expand::expand_word(word, &shell.env, shell.last_status))
            .collect()
    }

    #[test]
    fn substitution_captures_stdout() {
        let mut shell = Shell::with_env(Vec::<(String, String)>::new());
        assert_eq!(subst(&mut shell, "echo $(echo inner)"), ["echo", "inner"]);
        // Trailing newlines are stripped.
        assert_eq!(subst(&mut shell, "echo $(printf 'a\\n\\n')"), ["echo", "a"]);
        // Inside double quotes.
        assert_eq!(subst(&mut shell, "echo \"x$(echo y)z\""), ["echo", "xyz"]);
        // Nested substitution.
        assert_eq!(subst(&mut shell, "echo $(echo $(echo x))"), ["echo", "x"]);
        // Single quotes shield it.
        assert_eq!(subst(&mut shell, "echo '$(echo y)'"), ["echo", "$(echo y)"]);
        // Builtins run inside substitutions; variables expand there.
        let mut shell = Shell::with_env([("V".to_string(), "42".to_string())]);
        assert_eq!(subst(&mut shell, "echo $(echo $V)"), ["echo", "42"]);
    }

    #[test]
    fn substitution_failure_is_empty_and_status_is_unaffected() {
        let mut shell = Shell::with_env(Vec::<(String, String)>::new());
        assert_eq!(subst(&mut shell, "echo $(false)"), ["echo", ""]);
        assert_eq!(subst(&mut shell, "echo x$(no-such-command-z)y"), ["echo", "xy"]);
        // The failing substitution does not break the outer command.
        assert_eq!(run(&mut shell, "echo $(false)"), 0);
    }

    #[test]
    fn substitution_in_streaming_pipeline_and_redirect() {
        let mut shell = Shell::with_env(Vec::<(String, String)>::new());
        let dir = temp_dir("subst");
        let out = dir.join("out.txt");
        // Substitution inside an all-external (streaming) pipeline.
        let line = format!("echo $(echo streamed) | cat > {}", out.display());
        assert_eq!(run(&mut shell, &line), 0);
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "streamed\n");
        // Substitution producing the redirect target.
        let line = format!("echo data > $(echo {})", out.display());
        assert_eq!(run(&mut shell, &line), 0);
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "data\n");
        std::fs::remove_dir_all(&dir).ok();
    }
}
