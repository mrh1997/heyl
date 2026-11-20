//! The `--ui-cmd` protocol, against real shells.
//!
//! Each case runs once per shell this platform has: `sh` on Unix; `cmd`,
//! `powershell` and — when installed — `pwsh` on Windows. What is pinned is
//! the contract a user's command is written against (DESIGN.md §5):
//! the environment it gets, the line and the end of stdin when the wait is
//! over, `cancel` on stdout, an early exit that is not a cancel, and the
//! grace period after which a command ignoring its stdin is terminated.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use heyl_platform::{ApprovalConfig, ProcessApprovalUi, Shell};
use heyl_ports::{ApprovalOutcome, ApprovalRequest, ApprovalUi as _};

/// One way of writing each test command, in one shell's language.
struct Dialect {
    shell: Shell,
    /// Write `$HEYL_UI_SESSION|$HEYL_UI_MESSAGE`, then all of stdin, to a file.
    record: fn(&str) -> String,
    cancel: &'static str,
    linger: &'static str,
    quit: &'static str,
}

fn dialects() -> Vec<Dialect> {
    if cfg!(windows) {
        let mut all = vec![
            Dialect {
                shell: Shell::Cmd,
                record: |f| {
                    format!(r#"(echo %HEYL_UI_SESSION%^|%HEYL_UI_MESSAGE%& findstr "^") > "{f}""#)
                },
                cancel: "echo cancel",
                linger: "ping -n 30 127.0.0.1 >nul",
                quit: "exit 0",
            },
            powershell(Shell::PowerShell),
        ];
        if has("pwsh") {
            all.push(powershell(Shell::Pwsh));
        }
        all
    } else {
        vec![Dialect {
            shell: Shell::Sh,
            record: |f| format!(r#"{{ echo "$HEYL_UI_SESSION|$HEYL_UI_MESSAGE"; cat; }} > "{f}""#),
            cancel: "echo cancel",
            linger: "sleep 30",
            quit: "exit 0",
        }]
    }
}

fn powershell(shell: Shell) -> Dialect {
    Dialect {
        shell,
        record: |f| {
            format!(
                "\"$env:HEYL_UI_SESSION|$env:HEYL_UI_MESSAGE\" + \"`n\" + [Console]::In.ReadToEnd() \
                 | Set-Content -NoNewline -Path '{f}'"
            )
        },
        cancel: "Write-Output cancel",
        linger: "Start-Sleep -Seconds 30",
        quit: "exit 0",
    }
}

fn has(program: &str) -> bool {
    std::process::Command::new(program)
        .arg("-Version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

fn scratch(name: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    std::env::temp_dir().join(format!(
        "heyl-approval-{}-{}-{name}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

fn ui(command: String, shell: Option<Shell>) -> ProcessApprovalUi {
    ProcessApprovalUi::new(ApprovalConfig {
        window: true,
        command: Some(command),
        shell,
    })
}

fn unlock() -> ApprovalRequest<'static> {
    ApprovalRequest::Unlock {
        session: "agent",
        heading: "agent requests passwords",
        message: "approve it",
    }
}

#[tokio::test]
async fn the_command_gets_the_request_and_then_the_outcome() {
    for dialect in dialects() {
        let file = scratch("record.txt");
        let ui = ui(
            (dialect.record)(&file.to_string_lossy()),
            Some(dialect.shell),
        );

        let prompt = ui.show(unlock()).await;
        prompt.close(ApprovalOutcome::Approved).await;
        ui.settle().await;

        let recorded = std::fs::read_to_string(&file).unwrap_or_default();
        let lines: Vec<&str> = recorded.lines().map(str::trim).collect();
        assert_eq!(
            lines,
            ["agent|approve it", "approved"],
            "{:?}",
            dialect.shell
        );
        let _ = std::fs::remove_file(file);
    }
}

#[tokio::test]
async fn cancel_on_stdout_cancels() {
    for dialect in dialects() {
        let ui = ui(dialect.cancel.to_owned(), Some(dialect.shell));
        let prompt = ui.show(unlock()).await;
        let heard = tokio::time::timeout(Duration::from_secs(20), prompt.cancelled()).await;
        assert!(heard.is_ok(), "{:?} did not cancel", dialect.shell);
        prompt.close(ApprovalOutcome::Failed).await;
        ui.settle().await;
    }
}

#[tokio::test]
async fn an_early_exit_is_not_a_cancel() {
    for dialect in dialects() {
        let ui = ui(dialect.quit.to_owned(), Some(dialect.shell));
        let prompt = ui.show(unlock()).await;
        let heard = tokio::time::timeout(Duration::from_secs(3), prompt.cancelled()).await;
        assert!(
            heard.is_err(),
            "{:?} exiting read as a cancel",
            dialect.shell
        );
        prompt.close(ApprovalOutcome::Approved).await;
        ui.settle().await;
    }
}

#[tokio::test]
async fn a_command_ignoring_its_stdin_is_terminated_after_the_grace() {
    for dialect in dialects() {
        let ui = ui(dialect.linger.to_owned(), Some(dialect.shell));
        let prompt = ui.show(unlock()).await;
        // Let the shell get going, so the kill has a tree to take down.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let start = Instant::now();
        prompt.close(ApprovalOutcome::Approved).await;
        ui.settle().await;
        let took = start.elapsed();
        assert!(
            took >= Duration::from_secs(4) && took < Duration::from_secs(15),
            "{:?} took {took:?}",
            dialect.shell
        );
    }
}

/// A `.ps1` runs in `PowerShell` without `--ui-shell`: cmd would open it in an
/// editor.
#[cfg(windows)]
#[tokio::test]
async fn a_ps1_script_runs_in_powershell_by_itself() {
    let dir = scratch("ps1 dir");
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let script = dir.join("notify.ps1");
    let out = dir.join("out.txt");
    std::fs::write(
        &script,
        format!(
            "\"$env:HEYL_UI_SESSION|\" + [Console]::In.ReadToEnd() \
             | Set-Content -NoNewline -Path '{}'",
            out.display()
        ),
    )
    .expect("script");

    let ui = ui(format!("\"{}\"", script.display()), None);
    let prompt = ui.show(unlock()).await;
    prompt.close(ApprovalOutcome::Timeout).await;
    ui.settle().await;

    let recorded = std::fs::read_to_string(&out).unwrap_or_default();
    assert_eq!(recorded.trim(), "agent|timeout");
    let _ = std::fs::remove_dir_all(dir);
}

/// `--ui terminal` shows nothing, and a pairing never runs the command.
#[tokio::test]
async fn pairing_never_reaches_a_user_command() {
    let file = scratch("pair.txt");
    let record = format!(r#"echo ran > "{}""#, file.display());
    let ui = ProcessApprovalUi::new(ApprovalConfig {
        window: false,
        command: Some(record),
        shell: None,
    });
    let prompt = ui
        .show(ApprovalRequest::Pair {
            session: "agent",
            url: "https://heylogin.app/qr/#x",
            style: heyl_ports::QrStyle::Utf8,
        })
        .await;
    assert!(!prompt.is_shown());
    prompt.close(ApprovalOutcome::Approved).await;
    ui.settle().await;
    assert!(!file.exists());
}
