//! Registry for long-running shell commands started with
//! `bash(run_in_background=true)`: dev servers, watchers, long builds.
//!
//! Output from stdout and stderr is captured into a bounded buffer so the
//! model can read it incrementally (`BashOutput`) and stop the job
//! (`KillShell`). When a job exits on its own, a notice is queued so the next
//! turn can tell the model about it.
//!
//! Ported from `BackgroundJobs` in `AzureAiHarness`.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

const MAX_BUFFER_BYTES: usize = 1_000_000;
/// How often a monitor's new output is turned into a notice.
const MONITOR_POLL: Duration = Duration::from_millis(250);
/// Lines of one batch shown in its notice (the latest ones).
const MONITOR_MAX_LINES_PER_NOTICE: usize = 40;
/// A monitor printing more than this is stopped: it would flood the agent.
pub const MONITOR_MAX_LINES_PER_MINUTE: usize = 600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Running,
    Exited(Option<i32>),
    Killed,
}

impl std::fmt::Display for JobState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Running => f.write_str("running"),
            Self::Exited(Some(code)) => write!(f, "exited with code {code}"),
            Self::Exited(None) => f.write_str("exited (killed by signal)"),
            Self::Killed => f.write_str("killed"),
        }
    }
}

#[derive(Debug, Default)]
struct OutputBuffer {
    text: String,
    read_offset: usize,
    dropped: usize,
}

impl OutputBuffer {
    fn append(&mut self, line: &str) {
        self.text.push_str(line);
        self.text.push('\n');
        if self.text.len() > MAX_BUFFER_BYTES {
            let mut cut = self.text.len() - MAX_BUFFER_BYTES / 2;
            while !self.text.is_char_boundary(cut) {
                cut += 1;
            }
            self.text.drain(..cut);
            self.dropped += cut;
            self.read_offset = self.read_offset.saturating_sub(cut);
        }
    }
}

#[derive(Debug)]
struct Job {
    id: String,
    command: String,
    started: Instant,
    child: Mutex<Child>,
    output: Mutex<OutputBuffer>,
    state: Mutex<JobState>,
    /// Queue the usual "job exited" notice (monitors send their own).
    announce_exit: bool,
}

/// Snapshot of a job for listings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSummary {
    pub id: String,
    pub command: String,
    pub state: JobState,
    pub pid: u32,
    pub elapsed_secs: u64,
}

/// Result of reading a job's new output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRead {
    pub id: String,
    pub state: JobState,
    pub output: String,
    /// Bytes discarded from the buffer because the job produced too much output.
    pub dropped_bytes: usize,
}

#[derive(Debug, Default)]
pub struct BackgroundJobs {
    jobs: Mutex<BTreeMap<String, Arc<Job>>>,
    notices: Arc<Mutex<Vec<String>>>,
    next: Mutex<u64>,
}

/// Process-wide job registry shared by the bash tool, `BashOutput`,
/// `KillShell`, and the `/tasks` command.
pub fn global_background_jobs() -> &'static BackgroundJobs {
    static JOBS: OnceLock<BackgroundJobs> = OnceLock::new();
    JOBS.get_or_init(BackgroundJobs::default)
}

impl BackgroundJobs {
    /// Spawns `command` with piped output and registers it. Returns the job id.
    pub fn start(&self, command: Command, display: &str) -> std::io::Result<String> {
        self.start_job(command, display, true, false)
            .map(|(id, _)| id)
    }

    /// Like [`Self::start`], for the `Monitor` tool: each batch of new output
    /// lines is queued as a notice (so the agent can react to it, even while
    /// the prompt is idle), as is the exit. It is stopped after `timeout`, or
    /// when it prints more than [`MONITOR_MAX_LINES_PER_MINUTE`] lines a
    /// minute.
    pub fn start_monitor(
        &'static self,
        command: Command,
        display: &str,
        description: &str,
        timeout: Duration,
    ) -> std::io::Result<String> {
        let (id, _) = self.start_job(command, display, false, false)?;
        let poll_id = id.clone();
        let description = description.to_string();
        thread::spawn(move || self.watch_monitor(&poll_id, &description, timeout, true));
        Ok(id)
    }

    /// A monitor fed by the caller instead of a command: each line written
    /// to the returned stdin is an event (a `cat` job relays them, so
    /// TaskStop, the timeout, the flood limit and `/tasks` work as for any
    /// monitor). Used for `Monitor`'s WebSocket source. When the job is
    /// stopped, writes to the stdin fail; the feeder should then stop.
    pub fn start_monitor_feed(
        &'static self,
        display: &str,
        description: &str,
        timeout: Duration,
    ) -> std::io::Result<(String, ChildStdin)> {
        let (id, stdin) = self.start_job(Command::new("cat"), display, false, true)?;
        let stdin = stdin.ok_or_else(|| std::io::Error::other("no stdin for the monitor feed"))?;
        let poll_id = id.clone();
        let description = description.to_string();
        thread::spawn(move || self.watch_monitor(&poll_id, &description, timeout, true));
        Ok((id, stdin))
    }

    /// Watches the new output of an already running job (a
    /// `bash(run_in_background)` shell) as monitor notices. The job is not
    /// owned: at the timeout or the flood limit only the watching stops, and
    /// its exit is announced by the job itself. Shares the read position
    /// with `BashOutput`.
    pub fn attach_monitor(
        &'static self,
        id: &str,
        description: &str,
        timeout: Duration,
    ) -> Result<(), String> {
        let job = self
            .get(id)
            .ok_or_else(|| format!("no background shell with id {id}"))?;
        let state = *job.state.lock().unwrap_or_else(|e| e.into_inner());
        if state != JobState::Running {
            return Err(format!("background shell {id} is not running ({state})"));
        }
        let poll_id = id.to_string();
        let description = description.to_string();
        thread::spawn(move || self.watch_monitor(&poll_id, &description, timeout, false));
        Ok(())
    }

    /// `owned`: the monitor started the job, so it is killed at the timeout
    /// or flood limit and its exit is announced here.
    fn watch_monitor(&self, id: &str, description: &str, timeout: Duration, owned: bool) {
        let started = Instant::now();
        let mut window = (Instant::now(), 0_usize);
        loop {
            thread::sleep(MONITOR_POLL);
            let Ok(read) = self.read_new(id, Duration::ZERO) else {
                return;
            };
            let lines: Vec<&str> = read
                .output
                .lines()
                .filter(|l| !l.trim().is_empty())
                .collect();
            if !lines.is_empty() {
                if window.0.elapsed() > Duration::from_secs(60) {
                    window = (Instant::now(), 0);
                }
                window.1 += lines.len();
                let shown = lines.len().min(MONITOR_MAX_LINES_PER_NOTICE);
                let mut notice = format!(
                    "Monitor {id} ({description}):\n{}",
                    lines[lines.len() - shown..].join("\n")
                );
                if shown < lines.len() {
                    notice.push_str(&format!(
                        "\n[{} earlier lines left out]",
                        lines.len() - shown
                    ));
                }
                self.push_notice(notice);
                if window.1 > MONITOR_MAX_LINES_PER_MINUTE && !owned {
                    self.push_notice(format!(
                        "Stopped watching {id} ({description}): more than {MONITOR_MAX_LINES_PER_MINUTE} lines a minute. The shell keeps running; read it with BashOutput."
                    ));
                    return;
                }
                if window.1 > MONITOR_MAX_LINES_PER_MINUTE {
                    let _ = self.kill(id);
                    self.push_notice(format!(
                        "Monitor {id} ({description}) was stopped: more than {MONITOR_MAX_LINES_PER_MINUTE} lines a minute. Start it again with a tighter filter (grep --line-buffered …)."
                    ));
                    return;
                }
            }
            match read.state {
                JobState::Running if started.elapsed() >= timeout && !owned => {
                    self.push_notice(format!(
                        "Stopped watching {id} ({description}) after {}s; the shell keeps running.",
                        timeout.as_secs()
                    ));
                    return;
                }
                JobState::Running if started.elapsed() >= timeout => {
                    let _ = self.kill(id);
                    self.push_notice(format!(
                        "Monitor {id} ({description}) timed out after {}s and was stopped; start it again to keep watching.",
                        timeout.as_secs()
                    ));
                    return;
                }
                JobState::Running => {}
                JobState::Killed => return,
                // The shell announces its own exit.
                JobState::Exited(_) if !owned => return,
                JobState::Exited(_) => {
                    self.push_notice(format!(
                        "Monitor {id} ({description}) ended: {}.",
                        read.state
                    ));
                    return;
                }
            }
        }
    }

    fn start_job(
        &self,
        mut command: Command,
        display: &str,
        announce_exit: bool,
        piped_stdin: bool,
    ) -> std::io::Result<(String, Option<ChildStdin>)> {
        command
            .stdin(if piped_stdin {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // New process group so KillShell can stop the whole tree.
            command.process_group(0);
        }
        let mut child = command.spawn()?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let id = {
            let mut next = self.next.lock().unwrap_or_else(|e| e.into_inner());
            *next += 1;
            format!("bg{next}")
        };
        let job = Arc::new(Job {
            id: id.clone(),
            command: display.to_string(),
            started: Instant::now(),
            child: Mutex::new(child),
            output: Mutex::new(OutputBuffer::default()),
            state: Mutex::new(JobState::Running),
            announce_exit,
        });
        let mut readers = Vec::new();
        if let Some(stdout) = stdout {
            readers.push(spawn_reader(Arc::clone(&job), stdout));
        }
        if let Some(stderr) = stderr {
            readers.push(spawn_reader(Arc::clone(&job), stderr));
        }
        spawn_waiter(Arc::clone(&job), readers, Arc::clone(&self.notices));
        self.jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.clone(), job);
        Ok((id, stdin))
    }

    fn get(&self, id: &str) -> Option<Arc<Job>> {
        self.jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
    }

    #[must_use]
    pub fn list(&self) -> Vec<JobSummary> {
        self.jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|job| summarize(job))
            .collect()
    }

    /// Ids of all jobs that are still running.
    #[must_use]
    pub fn running_ids(&self) -> Vec<String> {
        self.list()
            .into_iter()
            .filter(|job| job.state == JobState::Running)
            .map(|job| job.id)
            .collect()
    }

    /// Returns output produced since the last read, waiting up to `wait` for
    /// new output or exit.
    pub fn read_new(&self, id: &str, wait: Duration) -> Result<JobRead, String> {
        let job = self.get(id).ok_or_else(|| self.unknown_job(id))?;
        let deadline = Instant::now() + wait;
        while Instant::now() < deadline && current_state(&job) == JobState::Running {
            let has_new = {
                let buffer = job.output.lock().unwrap_or_else(|e| e.into_inner());
                buffer.text.len() > buffer.read_offset
            };
            if has_new {
                // Let a burst of output finish before returning it.
                thread::sleep(Duration::from_millis(300));
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        Ok(take_new(&job))
    }

    /// The job and up to the last `max_bytes` of its output, without
    /// marking anything read (the model's `BashOutput` reads are separate).
    #[must_use]
    pub fn peek(&self, id: &str, max_bytes: usize) -> Option<(JobSummary, String)> {
        let job = self.get(id)?;
        let buffer = job.output.lock().unwrap_or_else(|e| e.into_inner());
        let mut start = buffer.text.len().saturating_sub(max_bytes);
        while !buffer.text.is_char_boundary(start) {
            start += 1;
        }
        let output = buffer.text[start..].to_string();
        drop(buffer);
        Some((summarize(&job), output))
    }

    /// Kills a job (and its process group) and returns its final output.
    pub fn kill(&self, id: &str) -> Result<JobRead, String> {
        let job = self.get(id).ok_or_else(|| self.unknown_job(id))?;
        if current_state(&job) == JobState::Running {
            *job.state.lock().unwrap_or_else(|e| e.into_inner()) = JobState::Killed;
            let mut child = job.child.lock().unwrap_or_else(|e| e.into_inner());
            kill_tree(&mut child);
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline
            && job
                .child
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .try_wait()
                .ok()
                .flatten()
                .is_none()
        {
            thread::sleep(Duration::from_millis(50));
        }
        Ok(take_new(&job))
    }

    /// Kills every running job (used on exit).
    pub fn kill_all(&self) {
        for id in self.running_ids() {
            let _ = self.kill(&id);
        }
    }

    /// Queues a notice for the agent, e.g. when a background sub-agent finishes.
    pub fn push_notice(&self, notice: String) {
        self.notices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(notice);
    }

    /// Whether notices are waiting to be delivered to the agent.
    #[must_use]
    pub fn has_notices(&self) -> bool {
        !self
            .notices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }

    /// Returns and clears notices about jobs that exited since the last call.
    pub fn drain_notices(&self) -> Vec<String> {
        std::mem::take(&mut *self.notices.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn unknown_job(&self, id: &str) -> String {
        let ids: Vec<String> = self.list().into_iter().map(|job| job.id).collect();
        if ids.is_empty() {
            format!("no background job `{id}`; no jobs have been started")
        } else {
            format!("no background job `{id}`; known jobs: {}", ids.join(", "))
        }
    }
}

fn current_state(job: &Job) -> JobState {
    *job.state.lock().unwrap_or_else(|e| e.into_inner())
}

fn summarize(job: &Job) -> JobSummary {
    JobSummary {
        id: job.id.clone(),
        command: job.command.clone(),
        state: current_state(job),
        pid: job.child.lock().unwrap_or_else(|e| e.into_inner()).id(),
        elapsed_secs: job.started.elapsed().as_secs(),
    }
}

fn take_new(job: &Job) -> JobRead {
    let mut buffer = job.output.lock().unwrap_or_else(|e| e.into_inner());
    let output = strip_ansi(&buffer.text[buffer.read_offset..]);
    buffer.read_offset = buffer.text.len();
    JobRead {
        id: job.id.clone(),
        state: current_state(job),
        output,
        dropped_bytes: buffer.dropped,
    }
}

fn spawn_reader<R: Read + Send + 'static>(job: Arc<Job>, stream: R) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let text = String::from_utf8_lossy(&line);
                    job.output
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .append(text.trim_end_matches(['\n', '\r']));
                }
            }
        }
    })
}

fn spawn_waiter(
    job: Arc<Job>,
    readers: Vec<thread::JoinHandle<()>>,
    notices: Arc<Mutex<Vec<String>>>,
) {
    thread::spawn(move || {
        let status = loop {
            let polled = job
                .child
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .try_wait();
            match polled {
                Ok(Some(status)) => break Some(status),
                Ok(None) => thread::sleep(Duration::from_millis(200)),
                Err(_) => break None,
            }
        };
        for reader in readers {
            let _ = reader.join();
        }
        let mut state = job.state.lock().unwrap_or_else(|e| e.into_inner());
        if *state == JobState::Killed {
            return; // stopped on purpose; nothing to report
        }
        *state = JobState::Exited(status.and_then(|s| s.code()));
        if !job.announce_exit {
            return;
        }
        let shown: String = job.command.chars().take(80).collect();
        notices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(format!(
                "Background job {} (`{shown}`) {}. Use BashOutput(bash_id=\"{}\") to read its remaining output.",
                job.id, *state, job.id
            ));
    });
}

fn kill_tree(child: &mut Child) {
    #[cfg(unix)]
    {
        if let Ok(pid) = i32::try_from(child.id()) {
            // Negative pid signals the whole process group created in `start`.
            let _ = Command::new("kill")
                .args(["-TERM", &format!("-{pid}")])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            thread::sleep(Duration::from_millis(200));
            if child.try_wait().ok().flatten().is_none() {
                let _ = Command::new("kill")
                    .args(["-KILL", &format!("-{pid}")])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }
    }
    let _ = child.kill();
}

fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.arg("-c").arg(script);
        command
    }

    #[test]
    fn captures_output_incrementally_and_reports_exit() {
        let jobs = BackgroundJobs::default();
        let id = jobs
            .start(
                sh("echo first; echo err >&2; sleep 0.3; echo second"),
                "demo",
            )
            .expect("start");
        let first = jobs.read_new(&id, Duration::from_secs(5)).expect("read");
        assert!(first.output.contains("first"), "{first:?}");

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut all = first.output;
        let mut state = first.state;
        while state == JobState::Running && Instant::now() < deadline {
            let read = jobs.read_new(&id, Duration::from_millis(500)).unwrap();
            all.push_str(&read.output);
            state = read.state;
        }
        all.push_str(&jobs.read_new(&id, Duration::ZERO).unwrap().output);
        assert!(all.contains("second") && all.contains("err"), "{all}");
        assert_eq!(state, JobState::Exited(Some(0)));
        let notices = jobs.drain_notices();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains(&id));
        assert!(jobs.drain_notices().is_empty());
    }

    #[test]
    fn kill_stops_job_without_notice() {
        let jobs = BackgroundJobs::default();
        let id = jobs.start(sh("sleep 30"), "sleeper").expect("start");
        assert_eq!(jobs.running_ids(), vec![id.clone()]);
        let read = jobs.kill(&id).expect("kill");
        assert_eq!(read.state, JobState::Killed);
        thread::sleep(Duration::from_millis(400));
        assert!(jobs.running_ids().is_empty());
        assert!(jobs.drain_notices().is_empty());
    }

    #[test]
    fn unknown_job_lists_known_ids() {
        let jobs = BackgroundJobs::default();
        let err = jobs.read_new("bg9", Duration::ZERO).unwrap_err();
        assert!(err.contains("no jobs have been started"));
    }

    #[test]
    fn buffer_is_bounded() {
        let mut buffer = OutputBuffer::default();
        let line = "x".repeat(1000);
        for _ in 0..2000 {
            buffer.append(&line);
        }
        assert!(buffer.text.len() <= MAX_BUFFER_BYTES);
        assert!(buffer.dropped > 0);
    }

    /// Notices of `jobs` until one contains `needle` (or 5 seconds pass).
    fn wait_notice(jobs: &BackgroundJobs, needle: &str, seen: &mut Vec<String>) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            seen.extend(jobs.drain_notices());
            if seen.iter().any(|notice| notice.contains(needle)) {
                return true;
            }
            thread::sleep(Duration::from_millis(50));
        }
        false
    }

    #[test]
    fn a_fed_monitor_turns_written_lines_into_notices_and_stops_cleanly() {
        use std::io::Write as _;
        let jobs: &'static BackgroundJobs = Box::leak(Box::default());
        let (id, mut feed) = jobs
            .start_monitor_feed("ws://demo", "demo socket", Duration::from_secs(30))
            .expect("start feed");
        writeln!(feed, "event one").expect("write");
        writeln!(feed, "event two").expect("write");
        let mut seen = Vec::new();
        assert!(wait_notice(jobs, "event two", &mut seen), "{seen:?}");
        assert!(seen
            .iter()
            .any(|n| n.contains(&format!("Monitor {id} (demo socket)"))));

        jobs.kill(&id).expect("stop");
        // The feeder learns the monitor is gone from a failed write.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut failed = false;
        while Instant::now() < deadline && !failed {
            failed = writeln!(feed, "late").and_then(|()| feed.flush()).is_err();
            thread::sleep(Duration::from_millis(50));
        }
        assert!(failed, "writes should fail once the monitor is stopped");
    }

    #[test]
    fn an_attached_monitor_stops_watching_at_timeout_but_leaves_the_shell_running() {
        let jobs: &'static BackgroundJobs = Box::leak(Box::default());
        let id = jobs
            .start(sh("echo tick1; sleep 0.3; echo tick2; sleep 30"), "ticker")
            .expect("start");
        jobs.attach_monitor(&id, "ticker", Duration::from_secs(1))
            .expect("attach");
        let mut seen = Vec::new();
        assert!(wait_notice(jobs, "tick2", &mut seen), "{seen:?}");
        assert!(wait_notice(jobs, "Stopped watching", &mut seen), "{seen:?}");
        assert!(
            jobs.running_ids().contains(&id),
            "the shell must keep running"
        );
        assert!(jobs
            .attach_monitor("bg-nope", "x", Duration::from_secs(1))
            .is_err());
        jobs.kill(&id).expect("kill");
        assert!(jobs
            .attach_monitor(&id, "again", Duration::from_secs(1))
            .is_err_and(|error| error.contains("not running")));
    }

    #[test]
    fn strips_ansi_sequences() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m plain"), "red plain");
    }
}
