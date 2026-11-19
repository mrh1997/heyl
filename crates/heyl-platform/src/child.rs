//! `heyl run`'s child process.
//!
//! heyl spawns the command after `--` with the resolved variables added to its
//! own environment, inherits stdin, stdout and stderr, and waits. Three things
//! make that more than a `Command::status`:
//!
//! * **Interrupts belong to the child.** Ctrl-C reaches every process attached
//!   to the terminal. Were heyl to die of it, the shell would get its prompt
//!   back while the child is still cleaning up and printing — so heyl ignores
//!   interrupts for as long as the child runs, and reports whatever the child
//!   made of them.
//! * **The exit code is the child's**, so `heyl run … -- make test` slots into
//!   scripts unchanged. A Unix child killed by a signal reports `128 + signal`,
//!   as a shell does.
//! * **Windows finds scripts.** `npm`, `yarn` and friends are `.cmd` files.
//!   `std::process::Command` only ever appends `.exe`, so `heyl run -- npm test`
//!   would fail where `npm test` works; [`resolve`] applies `PATHEXT` the way
//!   `cmd.exe` does.
//!
//! The variables are copied into the `Command` and the `Command` is dropped
//! right after the spawn: those copies are ordinary allocations, and the
//! shorter they live the better.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

/// Run `program` with `args`, adding `variables` to the inherited environment,
/// and wait for it.
///
/// Returns the child's exit code.
///
/// # Errors
/// The spawn's own error — typically `NotFound` for a command that does not
/// exist, `PermissionDenied` for one that cannot be executed.
pub async fn run<'a>(
    program: &OsStr,
    args: &[OsString],
    variables: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> std::io::Result<i32> {
    // Registered before the spawn, held until the wait ends: while a handler
    // is registered, an interrupt no longer terminates heyl.
    let _interrupts = IgnoreInterrupts::register();

    let mut command = tokio::process::Command::new(resolve(program));
    command.args(args).envs(variables);
    let mut child = command.spawn()?;
    drop(command);

    let status = child.wait().await?;
    Ok(code(status))
}

/// The exit code a shell would report for `status`.
#[cfg(unix)]
fn code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt as _;
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1)
}

/// The exit code a shell would report for `status`.
#[cfg(not(unix))]
fn code(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

/// Handlers that swallow interrupts, for as long as they are alive.
struct IgnoreInterrupts {
    #[cfg(unix)]
    _signals: Vec<tokio::signal::unix::Signal>,
    #[cfg(windows)]
    _ctrl_c: Option<tokio::signal::windows::CtrlC>,
    #[cfg(windows)]
    _ctrl_break: Option<tokio::signal::windows::CtrlBreak>,
}

impl IgnoreInterrupts {
    /// Best-effort: a handler that cannot be registered leaves heyl as
    /// interruptible as any other program, which is no reason not to run.
    fn register() -> Self {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Self {
                _signals: [SignalKind::interrupt(), SignalKind::quit()]
                    .into_iter()
                    .filter_map(|kind| signal(kind).ok())
                    .collect(),
            }
        }
        #[cfg(windows)]
        {
            Self {
                _ctrl_c: tokio::signal::windows::ctrl_c().ok(),
                _ctrl_break: tokio::signal::windows::ctrl_break().ok(),
            }
        }
    }
}

/// The program to spawn for `program`.
///
/// On Windows, a bare name without an extension is looked up on `PATH` with
/// every `PATHEXT` extension in turn, as `cmd.exe` does — the first file that
/// exists wins. Anything else, and every name on other platforms, is passed
/// through for the OS to find.
fn resolve(program: &OsStr) -> PathBuf {
    #[cfg(windows)]
    if let Some(found) = search_path(
        program,
        std::env::var_os("PATH").as_deref(),
        std::env::var_os("PATHEXT").as_deref(),
    ) {
        return found;
    }
    PathBuf::from(program)
}

/// `PATH` × `PATHEXT` lookup for a bare name, or `None` to leave it alone.
#[cfg(any(windows, test))]
fn search_path(program: &OsStr, path: Option<&OsStr>, pathext: Option<&OsStr>) -> Option<PathBuf> {
    let name = std::path::Path::new(program);
    // A path, or a name that already has an extension, is taken as given.
    if name.components().count() != 1 || name.extension().is_some() {
        return None;
    }
    let pathext = pathext
        .and_then(OsStr::to_str)
        .unwrap_or(".COM;.EXE;.BAT;.CMD");
    let extensions: Vec<&str> = pathext.split(';').filter(|e| !e.is_empty()).collect();

    std::env::split_paths(path?).find_map(|dir| {
        extensions.iter().find_map(|ext| {
            let mut file = program.to_os_string();
            file.push(ext);
            let candidate = dir.join(file);
            candidate.is_file().then_some(candidate)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("heyl-child-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn a_bare_name_finds_a_script_by_pathext() {
        let dir = scratch("pathext");
        std::fs::write(dir.join("tool.cmd"), "").expect("write");
        // Lower-case extensions keep the test meaningful on case-sensitive
        // file systems, where the suite also runs.
        let found = search_path(
            OsStr::new("tool"),
            Some(dir.as_os_str()),
            Some(OsStr::new(".exe;.cmd")),
        );
        assert_eq!(found, Some(dir.join("tool.cmd")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn earlier_extensions_win() {
        let dir = scratch("order");
        std::fs::write(dir.join("tool.exe"), "").expect("write");
        std::fs::write(dir.join("tool.cmd"), "").expect("write");
        let found = search_path(
            OsStr::new("tool"),
            Some(dir.as_os_str()),
            Some(OsStr::new(".exe;.cmd")),
        )
        .expect("found");
        assert_eq!(found, dir.join("tool.exe"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn paths_and_names_with_an_extension_are_left_alone() {
        let dir = scratch("alone");
        std::fs::write(dir.join("tool.cmd"), "").expect("write");
        for given in ["tool.cmd", "./tool", "sub/tool"] {
            assert_eq!(
                search_path(OsStr::new(given), Some(dir.as_os_str()), None),
                None,
                "{given}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_name_found_nowhere_is_left_for_the_os() {
        let dir = scratch("nowhere");
        assert_eq!(
            search_path(OsStr::new("no-such-tool"), Some(dir.as_os_str()), None),
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
