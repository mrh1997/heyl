//! The approval window, and `--ui-cmd`: one process protocol for both.
//!
//! Whatever shows a request runs as a **separate process** that heyl talks to
//! over its standard streams (DESIGN.md §5):
//!
//! * **in** — the environment: `HEYL_UI_SESSION`, `HEYL_UI_HEADING` (unlock
//!   only), `HEYL_UI_MESSAGE`;
//! * **done** — heyl writes one last line (`approved`, `timeout`, `failed`) and
//!   closes stdin. The process takes its UI down when stdin ends;
//! * **cancel** — the process prints `cancel` and heyl stops waiting;
//! * **safety net** — still running 5 s after stdin closed, its whole process
//!   tree is terminated. A process that exits early is not a cancel: a
//!   notifier that shows a popup and returns is a valid command.
//!
//! The built-in window is just the default process: `heyl` itself, re-run
//! with a hidden argument, on Windows and macOS (`heyl-cli`'s `window.rs`), and
//! [`LINUX_SCRIPT`] on Linux. Running it apart keeps the GUI stack — input
//! methods, accessibility hooks — out of the process holding the seed, and
//! leaves the main thread macOS demands of a UI free in that process.
//!
//! The built-in window additionally prints `shown` once it is on screen, so a
//! pairing knows whether the terminal has to draw the code as well.

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use heyl_ports::{ApprovalOutcome, ApprovalPrompt, ApprovalRequest, ApprovalUi};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// The built-in window on Linux: zenity, else kdialog.
///
/// A script rather than Rust because every Rust Wayland stack either links the
/// system `libwayland` — impossible in a static musl binary — or builds C,
/// which DESIGN.md §4 forbids. zenity and kdialog ship with the desktops.
pub const LINUX_SCRIPT: &str = include_str!("approval_ui.sh");

/// The hidden argument that makes `heyl` the built-in window (Windows, macOS).
pub const WINDOW_ARG: &str = "__approval-window";

/// How long a process may stay after stdin closed before it is terminated.
const GRACE: Duration = Duration::from_secs(5);

/// How long a pairing waits to hear that the window is up.
const SHOWN_WAIT: Duration = Duration::from_secs(3);

/// Which shell runs a `--ui-cmd`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    /// `sh -c` — the Unix default.
    Sh,
    /// `cmd /C` — the Windows default.
    Cmd,
    /// `powershell` (Windows `PowerShell` 5.1), no profile.
    PowerShell,
    /// `pwsh` (`PowerShell` 7), no profile.
    Pwsh,
}

impl Shell {
    /// The shell a command runs in when none was named.
    ///
    /// On Windows a command naming a `.ps1` script runs in `powershell`:
    /// `cmd` would open it in Notepad.
    #[must_use]
    pub fn default_for(command: &str) -> Self {
        if cfg!(windows) {
            if names_ps1(command) {
                Self::PowerShell
            } else {
                Self::Cmd
            }
        } else {
            Self::Sh
        }
    }
}

/// Whether a command's first word is a `.ps1` file.
fn names_ps1(command: &str) -> bool {
    let command = command.trim_start();
    let first = if let Some(rest) = command.strip_prefix('"') {
        rest.split('"').next().unwrap_or("")
    } else if let Some(rest) = command.strip_prefix('\'') {
        rest.split('\'').next().unwrap_or("")
    } else {
        command.split_whitespace().next().unwrap_or("")
    };
    first.to_ascii_lowercase().ends_with(".ps1")
}

/// What to show for each kind of request.
#[derive(Debug, Clone, Default)]
pub struct ApprovalConfig {
    /// Show the built-in window at all. `false` is `--ui terminal`.
    pub window: bool,
    /// `--ui-cmd`: shown instead of the window for unlock requests.
    pub command: Option<String>,
    /// `--ui-shell`: which shell runs it.
    pub shell: Option<Shell>,
}

/// The real adapter: a window, a command, or nothing.
pub struct ProcessApprovalUi {
    config: ApprovalConfig,
    /// Prompts that were told to close and may still be inside their grace.
    closing: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl ProcessApprovalUi {
    /// Build it from the options.
    #[must_use]
    pub fn new(config: ApprovalConfig) -> Self {
        Self {
            config,
            closing: Arc::default(),
        }
    }

    /// Wait for every closed prompt to be gone, terminating stragglers.
    ///
    /// Called as heyl exits, so a `--ui-cmd` that is cleaning up after the end
    /// of its stdin gets to finish rather than dying with heyl's runtime.
    pub async fn settle(&self) {
        let pending: Vec<_> = self
            .closing
            .lock()
            .map(|mut c| c.drain(..).collect())
            .unwrap_or_default();
        for task in pending {
            let _ = task.await;
        }
    }

    fn spawn(&self, request: &ApprovalRequest<'_>) -> Option<Command> {
        match request {
            ApprovalRequest::Unlock {
                session,
                heading,
                message,
            } => {
                let mut command = if let Some(text) = &self.config.command {
                    user_command(text, self.config.shell)
                } else if self.config.window {
                    built_in("unlock")?
                } else {
                    return None;
                };
                command
                    .env("HEYL_UI_SESSION", session)
                    .env("HEYL_UI_HEADING", heading)
                    .env("HEYL_UI_MESSAGE", message);
                Some(command)
            }
            // Never a `--ui-cmd`: a pairing code is not something heyl hands
            // to a program it did not write.
            ApprovalRequest::Pair { session, url, .. } => {
                if !self.config.window {
                    return None;
                }
                let mut command = built_in("pair")?;
                command
                    .env("HEYL_UI_SESSION", session)
                    .env(
                        "HEYL_UI_MESSAGE",
                        format!("Scan this code with the heylogin app to pair \"{session}\"."),
                    )
                    .env("HEYL_UI_URL", url);
                if cfg!(target_os = "linux") {
                    command.env("HEYL_UI_QR", crate::terminal::qr_text(url)?);
                }
                Some(command)
            }
            _ => None,
        }
    }
}

#[async_trait::async_trait]
impl ApprovalUi for ProcessApprovalUi {
    async fn show(&self, request: ApprovalRequest<'_>) -> Box<dyn ApprovalPrompt> {
        let Some(mut command) = self.spawn(&request) else {
            return Box::new(NoPrompt);
        };
        // A window that cannot start is not a reason to fail the command: the
        // terminal note is already on stderr.
        let Ok(prompt) = ChildPrompt::start(&mut command, Arc::clone(&self.closing)) else {
            return Box::new(NoPrompt);
        };
        if matches!(request, ApprovalRequest::Pair { .. }) {
            prompt.wait_shown().await;
        }
        Box::new(prompt)
    }
}

/// Shows nothing: `--ui terminal`, the scenario suite, and the fallback.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoApprovalUi;

#[async_trait::async_trait]
impl ApprovalUi for NoApprovalUi {
    async fn show(&self, _request: ApprovalRequest<'_>) -> Box<dyn ApprovalPrompt> {
        Box::new(NoPrompt)
    }
}

struct NoPrompt;

#[async_trait::async_trait]
impl ApprovalPrompt for NoPrompt {
    fn is_shown(&self) -> bool {
        false
    }

    async fn cancelled(&self) {
        std::future::pending::<()>().await;
    }

    async fn close(self: Box<Self>, _outcome: ApprovalOutcome) {}
}

/// The built-in window's process.
fn built_in(kind: &str) -> Option<Command> {
    if cfg!(target_os = "linux") {
        let mut command = Command::new("sh");
        command.arg("-c").arg(LINUX_SCRIPT).arg("heyl-ui").arg(kind);
        Some(command)
    } else {
        let mut command = Command::new(std::env::current_exe().ok()?);
        command.arg(WINDOW_ARG).arg(kind);
        Some(command)
    }
}

/// A `--ui-cmd` in its shell.
fn user_command(text: &str, shell: Option<Shell>) -> Command {
    let shell = shell.unwrap_or_else(|| Shell::default_for(text));
    match shell {
        Shell::Sh => {
            let mut command = Command::new("sh");
            command.arg("-c").arg(text);
            command
        }
        Shell::Cmd => cmd_command(text),
        Shell::PowerShell | Shell::Pwsh => {
            let mut command = Command::new(if shell == Shell::Pwsh {
                "pwsh"
            } else {
                "powershell"
            });
            // `& ` makes a quoted script path a call rather than a string —
            // needed only when the shell was inferred from a `.ps1`.
            let script = if names_ps1(text) && !text.trim_start().starts_with('&') {
                format!("& {text}")
            } else {
                text.to_owned()
            };
            command.args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                &script,
            ]);
            command
        }
    }
}

/// `cmd /C`, with the command line passed through untouched.
///
/// `cmd` does not parse its arguments the way every other Windows program
/// does, so Rust's quoting would mangle a command containing quotes. `/S`
/// with one pair of outer quotes is the form `cmd` documents for "run this
/// text as typed".
#[cfg(windows)]
fn cmd_command(text: &str) -> Command {
    let mut command = Command::new("cmd");
    command.raw_arg(format!("/D /S /C \"{text}\""));
    command
}

#[cfg(not(windows))]
fn cmd_command(text: &str) -> Command {
    let mut command = Command::new("cmd");
    command.args(["/D", "/C", text]);
    command
}

/// A running prompt process.
struct ChildPrompt {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    cancel: watch::Receiver<bool>,
    shown: watch::Receiver<bool>,
    closing: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl ChildPrompt {
    fn start(
        command: &mut Command,
        closing: Arc<Mutex<Vec<JoinHandle<()>>>>,
    ) -> std::io::Result<Self> {
        isolate(command);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Inherited, so a broken `--ui-cmd` can be debugged.
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();

        let (cancel_tx, cancel) = watch::channel(false);
        let (shown_tx, shown) = watch::channel(false);
        if let Some(stdout) = stdout {
            // Ends with the process's stdout, dropping both senders — which is
            // how a waiter learns that nothing more will come.
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    match line.trim() {
                        "shown" => {
                            let _ = shown_tx.send(true);
                        }
                        "cancel" => {
                            let _ = cancel_tx.send(true);
                        }
                        _ => {}
                    }
                }
            });
        }

        Ok(Self {
            child: Some(child),
            stdin,
            cancel,
            shown,
            closing,
        })
    }

    async fn wait_shown(&self) {
        let mut shown = self.shown.clone();
        let _ = tokio::time::timeout(SHOWN_WAIT, shown.wait_for(|s| *s)).await;
    }
}

#[async_trait::async_trait]
impl ApprovalPrompt for ChildPrompt {
    fn is_shown(&self) -> bool {
        *self.shown.borrow()
    }

    async fn cancelled(&self) {
        let mut cancel = self.cancel.clone();
        if cancel.wait_for(|c| *c).await.is_err() {
            // The process is gone without cancelling: it never will.
            std::future::pending::<()>().await;
        }
    }

    async fn close(mut self: Box<Self>, outcome: ApprovalOutcome) {
        if let Some(mut stdin) = self.stdin.take() {
            // Ignored: a process that already exited has closed its end.
            let _ = stdin
                .write_all(format!("{}\n", outcome.as_str()).as_bytes())
                .await;
            let _ = stdin.flush().await;
        }
        let Some(mut child) = self.child.take() else {
            return;
        };
        let task = tokio::spawn(async move {
            if tokio::time::timeout(GRACE, child.wait()).await.is_err() {
                kill_tree(&mut child).await;
            }
        });
        if let Ok(mut closing) = self.closing.lock() {
            closing.push(task);
        }
    }
}

/// Give the prompt a process tree of its own, and no console window.
fn isolate(command: &mut Command) {
    // Never hand heyl's own credentials to a prompt: the headless store and
    // the recovery code live in the environment it would otherwise inherit.
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("HEYL_") {
            command.env_remove(name);
        }
    }
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    {
        // CREATE_NO_WINDOW: a shell started from a heyl that has no console
        // would otherwise flash one up.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
}

/// Terminate a prompt and everything it started.
///
/// `cmd /C` and `sh -c` are only the root of what a `--ui-cmd` runs; killing
/// the root alone would leave its dialog on screen.
async fn kill_tree(child: &mut Child) {
    if let Some(pid) = child.id() {
        let mut killer = if cfg!(windows) {
            let mut c = Command::new("taskkill");
            c.args(["/PID", &pid.to_string(), "/T", "/F"]);
            c
        } else {
            let mut c = Command::new("kill");
            c.args(["-TERM", "--", &format!("-{pid}")]);
            c
        };
        let _ = killer
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    let _ = child.kill().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ps1_is_recognised_however_it_is_quoted() {
        assert!(names_ps1(r"C:\ui\notify.ps1"));
        assert!(names_ps1(r#""C:\my ui\Notify.PS1" -Quiet"#));
        assert!(names_ps1("'./notify.ps1'"));
        assert!(!names_ps1("notify-send hello.ps1"));
        assert!(!names_ps1("python notify.py"));
    }
}
