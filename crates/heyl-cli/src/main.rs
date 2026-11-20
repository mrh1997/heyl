//! `heyl` — the composition root.
//!
//! This is the **only** crate that names an adapter. `heyl-app` depends on the
//! port traits and on nothing that implements them, so it is physically
//! incapable of reaching an OS API or constructing a request; the wiring
//! happens here, once (DESIGN.md §4).

#[cfg(feature = "dev")]
mod api;
mod output;
#[cfg(not(target_os = "linux"))]
mod window;
mod wiring;

use std::sync::LazyLock;

use clap::{Parser, Subcommand};
use heyl_app::session::Slot;
use heyl_app::{AppError, ExitCode};

/// What `--version` prints: the crate version, and the commit it was built
/// from.
///
/// `HEYL_BUILD_SHA` is set by the artifact builds in `.github/workflows/ci.yml`
/// and `release.yml`. `option_env!` is what keeps a plain `cargo build` working
/// without it -- and printing no sha there is the honest answer, because a
/// local build is a working tree rather than a commit. The version is the date
/// of the last nightly release (DESIGN.md §7), which every commit after it
/// still carries, so the sha is what tells two binaries apart between releases.
///
/// A `LazyLock<String>` rather than a `const`: the two halves cannot be
/// concatenated at compile time without pulling in a crate for it, and passing
/// clap an owned `String` would mean enabling its `string` feature. Borrowing
/// from a `static` costs one lazy allocation at startup and leaves the
/// dependency surface alone.
static VERSION: LazyLock<String> = LazyLock::new(|| match option_env!("HEYL_BUILD_SHA") {
    Some(sha) => format!("{} ({sha})", env!("CARGO_PKG_VERSION")),
    None => env!("CARGO_PKG_VERSION").to_owned(),
});

/// Unofficial command-line client for heylogin.
#[derive(Debug, Parser)]
#[command(name = "heyl", version = VERSION.as_str(), about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Which session to act as.
    ///
    /// An agent gets its own identity by having `HEYL_SESSION` in the
    /// environment it is configured with; your shell keeps `default`.
    #[arg(long, global = true, env = "HEYL_SESSION")]
    session: Option<String>,

    /// How long to wait for a phone approval, in seconds.
    ///
    /// Without it, a command that needs an unlock waits indefinitely — the
    /// thing it is waiting for is a person. Named `--wait` rather than
    /// `--timeout` because on `session create` and `session set` a timeout is
    /// the *policy* being written, not the patience of this invocation.
    #[arg(long, global = true, value_name = "SECONDS")]
    wait: Option<u64>,

    /// How to draw a pairing code.
    ///
    /// Polarity is the silent failure: a code drawn for a dark terminal is a
    /// negative on a light one, renders perfectly, and will not scan.
    #[arg(long, global = true, value_enum, default_value_t = output::Qr::Utf8)]
    qr: output::Qr,

    /// How to print what a command has to say.
    ///
    /// `human` is prose and tables. `json` is one compact document per line,
    /// which is what lets a wrapper read `session create`'s pairing URL while
    /// the command is still waiting for a swipe; `json-pretty` is the same
    /// documents, indented. Global rather than per-verb: a caller sets it once
    /// and everything it drives answers in kind.
    ///
    /// **Not a compatibility promise yet.** The read path fixes the shape of
    /// every document at M6; until then a shape may change.
    #[arg(long, global = true, value_enum, default_value_t = output::Format::Human)]
    format: output::Format,

    /// Where to ask for a phone approval.
    ///
    /// `window` puts a small window in front of everything — centred, on top
    /// — that vanishes when the phone approves; its Cancel button aborts with
    /// exit code 4. For when heyl runs under a script or an agent and nobody
    /// is watching its terminal. The terminal note is printed either way, and
    /// a machine with no display falls back to it silently. On Linux the
    /// window is zenity, or kdialog on KDE (Wayland and X11 alike).
    #[arg(long, global = true, value_enum, env = "HEYL_UI", default_value_t = UiMode::Window)]
    ui: UiMode,

    /// Run this command to ask for an unlock approval, instead of the window.
    ///
    /// It gets `HEYL_UI_SESSION`, `HEYL_UI_HEADING` and `HEYL_UI_MESSAGE` in
    /// its environment.
    /// When the wait is over heyl writes one line to its stdin — `approved`,
    /// `timeout` or `failed` — and closes it: take your UI down then. Print
    /// `cancel` on stdout to abort the wait (exit code 4). Still running 5 s
    /// after its stdin closed, it is terminated with everything it started.
    /// Pairing (`session create`) never uses it.
    #[arg(long, global = true, env = "HEYL_UI_CMD", value_name = "COMMAND")]
    ui_cmd: Option<String>,

    /// The shell `--ui-cmd` runs in.
    ///
    /// Default: `sh` on Linux and macOS; on Windows `cmd`, or `powershell`
    /// when the command names a `.ps1` script.
    #[arg(long, global = true, value_enum, env = "HEYL_UI_SHELL")]
    ui_shell: Option<UiShell>,
}

/// `--ui`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum UiMode {
    /// A window, falling back to the terminal.
    Window,
    /// The terminal only.
    Terminal,
}

/// `--ui-shell`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum UiShell {
    /// `sh -c`.
    Sh,
    /// `cmd /C` (Windows).
    Cmd,
    /// Windows `PowerShell` 5.1, no profile.
    Powershell,
    /// `PowerShell` 7, no profile.
    Pwsh,
}

impl From<UiShell> for heyl_platform::Shell {
    fn from(value: UiShell) -> Self {
        match value {
            UiShell::Sh => Self::Sh,
            UiShell::Cmd => Self::Cmd,
            UiShell::Powershell => Self::PowerShell,
            UiShell::Pwsh => Self::Pwsh,
        }
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Recover access with a recovery code, and store a session.
    ///
    /// Present only in a build made with `--features dev`. Not because it is
    /// dangerous — it is, and it asks — but because it is not the product: it
    /// exists because reaching a session before the phone swipe worked is how
    /// the hierarchy was first confirmed. Someone who has lost their phone
    /// should recover in heylogin's own app, which does it without a
    /// third-party client in the path (DESIGN.md §2).
    ///
    /// This is **not** a login. heylogin treats a recovery code as an account
    /// recovery: the server disconnects your phone's authenticator and its
    /// locks, and pairing a phone again afterwards regenerates every profile.
    /// You will be shown what is about to be disconnected, and asked, unless
    /// there is nothing left to lose.
    ///
    /// The code is read from `HEYL_RECOVERY_CODE`, or prompted for without
    /// echo, or read from stdin when piped. There is deliberately **no
    /// `--code` flag**: a recovery code unlocks every vault, so putting it in
    /// argv would leak it into `ps` output and shell history.
    #[cfg(feature = "dev")]
    Recovery {
        /// The account's email address.
        #[arg(long, env = "HEYL_EMAIL")]
        email: Option<String>,

        /// Proceed without asking.
        ///
        /// Required to disconnect anything when there is no terminal to ask
        /// at — a destructive operation does not run silently just because
        /// nobody was there to object.
        #[arg(long)]
        confirm: bool,
    },

    /// UNSAFE — heylogin's gRPC surface by hand, with no guards.
    ///
    /// Every one of the 123 RPCs is reachable by name. `CreateTokens` with a
    /// `BACKUP_CODE` signature performs the destructive recovery `heyl
    /// recovery` asks about — except nothing asks. `api derive` prints seeds
    /// and vault keys to the terminal. **Point it at a throwaway account.**
    ///
    /// Present only in a build made with `--features dev`, which is what keeps
    /// `prost-reflect` and the embedded descriptor out of the release
    /// dependency graph (DESIGN.md §5). It is listed here rather than hidden:
    /// a command the binary actually has should say so.
    #[cfg(feature = "dev")]
    Api {
        #[command(subcommand)]
        command: api::Api,
    },

    /// Read a login from a vault — a whole login, or one field.
    ///
    /// Selectors are flags, not positionals. A vault is optional: without one,
    /// every readable login vault is searched, personal first. A login is
    /// required. A field is optional: without one, every non-empty field is
    /// printed as `name: value`; with `-f`/`-F`, just that value goes to stdout,
    /// raw, so `$(heyl get -l x -f password)` is exact.
    ///
    /// `-l` matches a login's display name, title, or a website — exact and
    /// case-insensitive. When several match, the one the heylogin app shows
    /// first is returned (most recently changed), and a note on stderr says how
    /// many matched; `-L <uuid>` picks one exactly.
    Get {
        /// Vault by name: `private`, `inbox`, an organisation, or a team.
        #[arg(short = 'v', long = "vault-name", value_name = "NAME")]
        vault_name: Option<String>,

        /// Vault by id.
        #[arg(short = 'V', long = "vault-id", value_name = "UUID")]
        vault_id: Option<String>,

        /// Login by display name, title, or website.
        #[arg(short = 'l', long = "login-title", value_name = "TITLE")]
        login_title: Option<String>,

        /// Login by id.
        #[arg(short = 'L', long = "login-id", value_name = "UUID")]
        login_id: Option<String>,

        /// Print one field by name — a built-in (`password`, `username`, …) or
        /// a custom field. A built-in wins a clash with a custom field.
        #[arg(short = 'f', long = "field-name", value_name = "NAME")]
        field_name: Option<String>,

        /// Print one custom field by id.
        #[arg(short = 'F', long = "field-id", value_name = "UUID")]
        field_id: Option<String>,
    },

    /// Run a command with a project's secrets in its environment.
    ///
    /// Every live login whose name, title or website is `<project>#<VARIABLE>`
    /// becomes one variable: the
    /// part after the `#` is its name, the login's password its value. So with
    /// `--project github.com/me/app/prod`, a login named
    /// `github.com/me/app/prod#DATABASE_URL` sets `DATABASE_URL`.
    ///
    /// Without `-v`/`-V`, every readable login vault except the inbox is
    /// searched; with one, only that vault. The command after `--` inherits
    /// heyl's environment plus these variables, and its exit code is heyl's.
    /// The values never reach disk, argv or stdout.
    Run {
        /// The project, e.g. `github.com/me/app` or `github.com/me/app/prod`.
        #[arg(long, value_name = "PROJECT")]
        project: String,

        /// Search only this vault, by name: `private`, `inbox`, an
        /// organisation, or a team.
        #[arg(short = 'v', long = "vault-name", value_name = "NAME")]
        vault_name: Option<String>,

        /// Search only this vault, by id.
        #[arg(short = 'V', long = "vault-id", value_name = "UUID")]
        vault_id: Option<String>,

        /// The command to run, after `--`.
        #[arg(last = true, required = true, value_name = "COMMAND")]
        command: Vec<std::ffi::OsString>,
    },

    /// Sessions: this machine's devices on the account.
    ///
    /// A session is what your phone approves, and what it names when it asks.
    /// Each has its own keys, its own unlock policy and its own entry in the
    /// heylogin app, so an agent and you can hold opposite policies at once.
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },

    /// Check the key hierarchy against the backend, link by link.
    ///
    /// Present only in a build made with `--features dev`. It answers a
    /// question whose only possible action is a code change — "does the
    /// reverse-engineered derivation agree with heylogin?" — which is what
    /// makes it a workbench command rather than a diagnostic. A release build
    /// answers the question a user can act on with `session list`.
    ///
    /// Recovers the seed from this session's unlock grant, derives every key,
    /// compares each against the public half heylogin publishes, then opens
    /// every vault. This is what confirms that the reverse-engineered
    /// derivation actually agrees with heylogin.
    #[cfg(feature = "dev")]
    Doctor,
}

/// The `heyl session` verbs.
#[derive(Debug, Subcommand)]
enum SessionCommand {
    /// Pair a new session with a QR scan, and register it as a device.
    ///
    /// One scan per session, deliberately: every session's seed comes straight
    /// from the phone, so no session can conjure another. The new session is
    /// born **locked** unless `--unlock` is given — pairing establishes an
    /// identity, approving an unlock is a separate act.
    Create {
        /// The slot name. Defaults to `default`.
        name: Option<String>,

        /// What the phone shows for this device.
        ///
        /// Defaults to the slot name, or `heyl CLI` for the default slot. This
        /// is the **only** string the approval screen displays, which is why
        /// naming a slot for its caller is worth doing.
        #[arg(long)]
        display: Option<String>,

        /// How long an approval lasts: `90s`, `30m`, `8h`.
        ///
        /// Server-enforced: the backend stops serving the unlock at the
        /// deadline, so it binds any client that discards the seed. One minute
        /// is its floor.
        #[arg(long, value_name = "DURATION")]
        timeout: Option<String>,

        /// Drop the unlock when each command exits, so the next one re-asks.
        #[arg(long)]
        strict: bool,

        /// Slide the unlock window on use, instead of expiring absolutely.
        #[arg(long)]
        auto_extend: bool,

        /// Self-grant an unlock from the swipe you just did.
        #[arg(long)]
        unlock: bool,
    },

    /// Ask the phone to unlock a session, and wait for the approval.
    ///
    /// Blocks until you approve. `--wait` bounds it.
    Unlock {
        /// The slot name.
        name: Option<String>,
    },

    /// Drop a session's unlock now, and cancel any request pending on it.
    ///
    /// Works on any session of the account, not only this machine's.
    Lock {
        /// The slot name.
        name: Option<String>,
    },

    /// Change one setting: `set [SLOT] <KEY> <VALUE>`.
    ///
    /// `display-name` is vault content, so it may ask for an unlock. `timeout`,
    /// `strict` and `auto-extend` ride on the session record and never do.
    ///
    /// `KEY` is `display-name`, `timeout`, `strict` or `auto-extend`; `VALUE`
    /// is `on`/`off` for the flags and a duration for `timeout`. `SLOT`
    /// defaults to the selected one, so `session set strict on` and
    /// `session set ci strict on` both work.
    ///
    /// The three arrive as one list rather than as three arguments, because an
    /// optional positional in front of required ones is ambiguous — and clap
    /// says so, with a debug assertion that made this command panic outright.
    Set {
        /// `[SLOT] <KEY> <VALUE>`.
        #[arg(value_name = "ARGS", num_args = 2..=3, required = true)]
        args: Vec<String>,
    },

    /// Read settings back. Never unlocks.
    Get {
        /// The slot name.
        name: Option<String>,
        /// One key, or every readable one.
        key: Option<String>,
    },

    /// Retire a session: tombstone its device entry, delete it, forget its keys.
    Remove {
        /// The slot name.
        name: Option<String>,

        /// Delete what can be deleted even if the device entry cannot be
        /// tombstoned, leaving the app listing a device that no longer exists.
        #[arg(long)]
        force: bool,
    },

    /// Every session this machine has, and what the backend says about them.
    List,
}

fn main() -> std::process::ExitCode {
    // The approval window is this binary run again (`heyl_platform::approval_ui`).
    // Recognised before clap, so it is no subcommand anyone sees, and before
    // the hardening and the runtime, which a window holding no secret needs
    // neither of.
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() == Some(std::ffi::OsStr::new(heyl_platform::approval_ui::WINDOW_ARG))
    {
        let kind = args.next().unwrap_or_default();
        return approval_window(&kind.to_string_lossy());
    }

    // Before anything can reach for a secret: no core dumps, and memory
    // locking must actually work. Failing here is deliberate -- see
    // `heyl_platform::process`.
    if let Err(e) = heyl_platform::harden_process() {
        eprintln!("heyl: {e}");
        return std::process::ExitCode::from(ExitCode::Failure as u8);
    }

    // `current_thread`: measured ~650 µs cheaper per invocation than
    // `multi_thread`, with an identical binary size (DESIGN.md §4).
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("heyl: could not start the async runtime: {e}");
            return std::process::ExitCode::from(ExitCode::Failure as u8);
        }
    };

    match runtime.block_on(run(Cli::parse())) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("heyl: {e}");
            std::process::ExitCode::from(e.exit_code() as u8)
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn approval_window(kind: &str) -> std::process::ExitCode {
    window::run(kind)
}

/// Linux's window is a script; this binary has none to show.
#[cfg(target_os = "linux")]
fn approval_window(_kind: &str) -> std::process::ExitCode {
    std::process::ExitCode::from(3)
}

async fn run(cli: Cli) -> Result<std::process::ExitCode, AppError> {
    let adapters = wiring::Adapters::new(heyl_platform::ApprovalConfig {
        window: cli.ui == UiMode::Window,
        command: cli.ui_cmd.clone().filter(|c| !c.trim().is_empty()),
        shell: cli.ui_shell.map(Into::into),
    })?;
    let result = dispatch(cli, &adapters).await;
    // A `--ui-cmd` gets its grace period even when heyl is done first.
    adapters.settle().await;
    result
}

async fn dispatch(
    cli: Cli,
    adapters: &wiring::Adapters,
) -> Result<std::process::ExitCode, AppError> {
    let ports = adapters.ports();
    let slot = Slot::new(cli.session.as_deref());

    match cli.command {
        #[cfg(feature = "dev")]
        Command::Api { command } => match api::run(command).await {
            Ok(()) => Ok(std::process::ExitCode::SUCCESS),
            // Reported here rather than mapped into `AppError`: this is not a
            // use case, and the core should not learn a vocabulary for a tool
            // that is not shipped.
            Err(e) => {
                eprintln!("heyl: {e}");
                Ok(std::process::ExitCode::from(ExitCode::Failure as u8))
            }
        },

        #[cfg(feature = "dev")]
        Command::Recovery { email, confirm } => {
            let email = match email {
                Some(email) => email,
                None => ports.terminal.prompt_line("heylogin email: ")?,
            };
            let outcome = heyl_app::recovery::run(
                &ports,
                &email,
                if confirm {
                    heyl_app::recovery::Confirmation::Granted
                } else {
                    heyl_app::recovery::Confirmation::Ask("Disconnect and recover? [y/N] ")
                },
                wiring::code_source(),
                heyl_domain::ChallengeEncoding::Utf8,
                heyl_domain::SessionType::BackupCode,
            )
            .await?;
            output::recovery(&outcome, cli.format);
            Ok(std::process::ExitCode::SUCCESS)
        }

        Command::Get {
            vault_name,
            vault_id,
            login_title,
            login_id,
            field_name,
            field_id,
        } => {
            let query = heyl_app::get::Query::from_flags(
                vault_name,
                vault_id,
                login_title,
                login_id,
                field_name,
                field_id,
            )?;
            let located = heyl_app::get::run(&ports, &slot, &query, cli.wait).await?;
            // A strict slot re-locks as the command exits, so the next access
            // asks the phone again.
            heyl_app::session::finish(&ports, &slot).await;
            output::get(&located, cli.format);
            Ok(std::process::ExitCode::SUCCESS)
        }

        Command::Run {
            project,
            vault_name,
            vault_id,
            command,
        } => {
            run_project(
                &ports, &slot, cli.wait, &project, vault_name, vault_id, &command,
            )
            .await
        }

        Command::Session { command } => {
            session(&ports, command, &slot, cli.wait, cli.qr.into(), cli.format).await
        }

        #[cfg(feature = "dev")]
        Command::Doctor => {
            // An ordinary command: a locked slot asks the phone and waits,
            // rather than failing (decision 16).
            let session = heyl_app::unlock::ensure(&ports, &slot, cli.wait).await?;
            let report = heyl_app::doctor::run_with(&ports, session).await?;
            // Strict slots re-lock as the command exits, so the next access
            // asks the phone again.
            heyl_app::session::finish(&ports, &slot).await;
            output::doctor(&report, cli.format);
            Ok(if report.has_failures() {
                std::process::ExitCode::from(ExitCode::Failure as u8)
            } else {
                std::process::ExitCode::SUCCESS
            })
        }
    }
}

/// `heyl run`: resolve the project's variables, then run the command with
/// them and exit as it did.
async fn run_project(
    ports: &heyl_app::Ports<'_>,
    slot: &Slot,
    wait: Option<u64>,
    project: &str,
    vault_name: Option<String>,
    vault_id: Option<String>,
    command: &[std::ffi::OsString],
) -> Result<std::process::ExitCode, AppError> {
    let project = heyl_app::run::Project::parse(project)?;
    let vault = heyl_app::get::VaultSelector::from_flags(vault_name, vault_id)?;
    let variables = heyl_app::run::run(ports, slot, &project, &vault, wait).await?;
    // Before the child starts, not after it exits: a strict slot must
    // not stay unlocked for as long as a build runs (DESIGN.md §5).
    heyl_app::session::finish(ports, slot).await;
    output::run_notes(&variables);

    let (program, args) = command
        .split_first()
        .expect("clap requires at least one word after --");
    let outcome = heyl_platform::child::run(
        program,
        args,
        variables
            .iter()
            .map(|v| (v.name.as_str(), v.value.as_str())),
    )
    .await;
    // The values are wiped here, before heyl reports anything.
    drop(variables);

    match outcome {
        Ok(code) => Ok(child_exit(code)),
        Err(e) => {
            eprintln!("heyl: cannot run {}: {e}", program.to_string_lossy());
            // A shell's codes for "not found" and "not executable", so
            // a script cannot mistake them for the child's own exit.
            Ok(std::process::ExitCode::from(
                if e.kind() == std::io::ErrorKind::NotFound {
                    127
                } else {
                    126
                },
            ))
        }
    }
}

/// heyl's exit code for a child that exited with `code`.
///
/// Codes in `0..=255` pass through. Windows reports wider ones — an NTSTATUS
/// such as `0xC000013A` for a Ctrl-C — which `ExitCode` cannot carry, so those
/// leave through `process::exit` with the value intact. Nothing secret is
/// live by then: the variables were dropped before this is reached.
fn child_exit(code: i32) -> std::process::ExitCode {
    u8::try_from(code).map_or_else(|_| std::process::exit(code), std::process::ExitCode::from)
}

/// Dispatch one `heyl session` verb.
///
/// The slot a verb acts on is its positional argument when given, and the
/// globally selected one otherwise — so `heyl session lock` locks whatever
/// `HEYL_SESSION` points at, and `heyl session lock ci` always locks `ci`.
async fn session(
    ports: &heyl_app::Ports<'_>,
    command: SessionCommand,
    selected: &Slot,
    timeout: Option<u64>,
    qr: heyl_ports::QrStyle,
    format: output::Format,
) -> Result<std::process::ExitCode, AppError> {
    use heyl_app::session;

    let pick = |name: Option<String>| -> Slot {
        name.map_or_else(|| selected.clone(), |n| Slot::new(Some(&n)))
    };

    match command {
        SessionCommand::Create {
            name,
            display,
            timeout,
            strict,
            auto_extend,
            unlock,
        } => {
            let slot = pick(name);
            let policy = heyl_domain::SessionPolicy {
                timeout_minutes: match timeout.as_deref() {
                    Some(value) => session::parse_timeout(value)?,
                    None if strict => heyl_domain::MIN_TIMEOUT_MINUTES,
                    None => heyl_domain::DEFAULT_TIMEOUT_MINUTES,
                },
                strict,
                auto_extend,
            };
            // The pairing URL reaches the caller from inside `create`,
            // before it blocks on a person: a wrapper driving documents can
            // draw its own code or open the link while the swipe is pending.
            let created = session::create(
                ports,
                &slot,
                display.as_deref(),
                policy,
                unlock,
                qr,
                &|url| output::pairing_url(url, format),
            )
            .await?;
            output::session_created(&slot, &created, policy, format);
        }

        SessionCommand::Unlock { name } => {
            let slot = pick(name);
            let until = session::unlock_and_wait(ports, &slot, timeout).await?;
            output::session_unlocked(&slot, until, format);
        }

        SessionCommand::Lock { name } => {
            let slot = pick(name);
            session::lock(ports, &slot).await?;
            output::session_locked(&slot, format);
        }

        SessionCommand::Set { args } => {
            // Two arguments name the setting on the selected slot; three name
            // the slot first. `num_args` has already refused anything else.
            let (slot, key, value) = match args.as_slice() {
                [key, value] => (selected.clone(), key, value),
                [name, key, value] => (Slot::new(Some(name)), key, value),
                _ => unreachable!("clap accepts two or three"),
            };
            let setting = session::Setting::parse(key)?;
            session::set(ports, &slot, setting, value).await?;
            output::session_set(&slot, setting, value, format);
        }

        SessionCommand::Get { name, key } => {
            let slot = pick(name);
            let setting = key.as_deref().map(session::Setting::parse).transpose()?;
            let values = session::get(ports, &slot, setting).await?;
            output::session_settings(&slot, &values, format);
        }

        SessionCommand::Remove { name, force } => {
            let slot = pick(name);
            let removed = session::remove(ports, &slot, force).await?;
            output::session_removed(&slot, &removed, format);
        }

        SessionCommand::List => {
            let slots = session::slots(ports).await;
            let statuses = session::list(ports, &slots).await?;
            output::session_list(&statuses, format);
        }
    }

    Ok(std::process::ExitCode::SUCCESS)
}
