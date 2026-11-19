//! `heyl run` — a project's secrets, by naming convention.
//!
//! There is no template file. A project is written like a web page without a
//! scheme, e.g. `github.com/mrh1997/heyl/prod`; every live login whose name starts
//! with it followed by `#` contributes one environment variable — the
//! part of the name after the `#` is the variable, the login's password its
//! value. DESIGN.md §5 (`run`) holds every rule; this module resolves the
//! variables, and `heyl-cli` spawns the child with them.
//!
//! Secrets leave as [`Zeroizing`] strings: the CLI copies them into the child's
//! environment and drops them, which is as far as a process can carry them.

use std::collections::BTreeMap;

use heyl_domain::{ProtectedSecret, VaultType};
use heyl_vault::login::{Liveness, Login};
use zeroize::Zeroizing;

use crate::get::{self, VaultSelector};
use crate::{AppError, Ports};

/// A validated `--project` value, and the prefix it searches for.
#[derive(Debug, Clone)]
pub struct Project {
    prefix: String,
}

impl Project {
    /// Validate a `--project` value.
    ///
    /// # Errors
    /// [`AppError::BadProject`] (exit 7) if it is empty, has a scheme such as
    /// `heyl://` (project logins are named like a web page without one, so it
    /// could never match), ends in `/`, or contains a `#` — the separator heyl
    /// appends, so one inside would make the prefix ambiguous.
    pub fn parse(project: &str) -> Result<Self, AppError> {
        let reason = if project.is_empty() {
            Some("it is empty")
        } else if project.contains("://") {
            Some("it must not have a scheme; write it like github.com/owner/repo")
        } else if project.ends_with('/') {
            Some("it must not end with /")
        } else if project.contains('#') {
            Some("it must not contain #")
        } else {
            None
        };
        match reason {
            Some(reason) => Err(AppError::BadProject { reason }),
            None => Ok(Self {
                prefix: format!("{project}#"),
            }),
        }
    }

    /// The prefix a login's name must start with: the project and a `#`.
    #[must_use]
    pub fn prefix(&self) -> &str {
        &self.prefix
    }
}

/// One environment variable for the child.
pub struct Variable {
    /// Its name — the part of the login's name after the `#`.
    pub name: String,
    /// The login's password.
    pub value: Zeroizing<String>,
    /// How many live logins yielded this name; above 1, the ranking chose.
    pub candidates: usize,
}

impl std::fmt::Debug for Variable {
    // Hand-written so a value can never reach a log or a panic message.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Variable")
            .field("name", &self.name)
            .field("value", &"<redacted>")
            .field("candidates", &self.candidates)
            .finish()
    }
}

/// Resolve a project's variables.
///
/// Unlocks like every read, asking the phone and waiting if needed (bounded by
/// `wait`). The result is sorted by variable name.
///
/// # Errors
/// [`AppError::NoProjectSecrets`] (exit 2) when no login carries the prefix;
/// [`AppError::InvalidVariableName`] (exit 7) when one yields a name that is
/// not a portable environment variable; [`AppError::NoSuchVault`] and the
/// vault-open errors for a named vault; the usual unlock and backend errors.
pub async fn run(
    ports: &Ports<'_>,
    slot: &crate::session::Slot,
    project: &Project,
    vault: &VaultSelector,
    wait: Option<u64>,
) -> Result<Vec<Variable>, AppError> {
    let session = crate::unlock::ensure(ports, slot, wait).await?;
    let scope = get::resolve_scope(ports, &session, vault).await?;
    let named = !matches!(vault, VaultSelector::Any);

    let mut found: Vec<Found<ProtectedSecret>> = Vec::new();
    for summary in scope {
        // An inbox holds what other people sent you, and a login's name
        // decides a variable's name: searched only when asked for by name.
        if !named && summary.vault_type == VaultType::Inbox {
            continue;
        }
        let opened = match get::open_vault(ports, &session, summary).await {
            Ok(opened) => opened,
            Err(e) if named => return Err(e),
            // As for `get`: a vault another client wrote in a form we cannot
            // read must not stop the search.
            Err(_) => continue,
        };
        for login in heyl_vault::login::logins(&opened.document) {
            if let Some(variable) = variable_name(&login, project.prefix()) {
                found.push(Found {
                    variable: variable.to_owned(),
                    tier: get::tier(summary.vault_type),
                    login,
                    payload: opened.protected.clone(),
                });
            }
        }
    }

    let mut variables = Vec::new();
    for chosen in choose(found)? {
        let value = match &chosen.found.login.password {
            Some(password) => get::decrypt(&chosen.found.payload, password)?,
            None => String::new(),
        };
        variables.push(Variable {
            name: chosen.found.variable,
            value: Zeroizing::new(value),
            candidates: chosen.candidates,
        });
    }
    Ok(variables)
}

/// A login that carries the prefix, before anything is decrypted.
///
/// Generic over what decrypts it, so the selection rules are testable without
/// a key.
struct Found<T> {
    variable: String,
    tier: u8,
    login: Login,
    payload: T,
}

/// The winner for one variable, and how many it beat.
struct Chosen<T> {
    found: Found<T>,
    candidates: usize,
}

/// The variable a login yields, if its name, title or a website carries the
/// prefix.
///
/// Case-sensitive: what follows the `#` becomes a variable name verbatim. The
/// display name is tried first, as it is what the app shows, then the title,
/// then each website in order. A project login is written like a web address,
/// so the website field is where the app most naturally stores it — and a
/// website may carry a leading `https://` or `http://` the project does not.
fn variable_name<'a>(login: &'a Login, prefix: &str) -> Option<&'a str> {
    let names = login
        .display_headline
        .as_deref()
        .into_iter()
        .chain(std::iter::once(login.title.as_str()));
    let websites = login.websites.iter().map(|w| {
        w.strip_prefix("https://")
            .or_else(|| w.strip_prefix("http://"))
            .unwrap_or(w)
    });
    names
        .chain(websites)
        .find_map(|name| name.strip_prefix(prefix))
}

/// Apply DESIGN.md §5's selection rules: live logins only, every name valid,
/// at least one match, and one winner per variable by `get`'s ranking.
fn choose<T>(found: Vec<Found<T>>) -> Result<Vec<Chosen<T>>, AppError> {
    let live: Vec<Found<T>> = found
        .into_iter()
        .filter(|f| f.login.liveness() == Liveness::Live)
        .collect();
    if live.is_empty() {
        return Err(AppError::NoProjectSecrets);
    }
    if let Some(bad) = live.iter().find(|f| !is_variable_name(&f.variable)) {
        return Err(AppError::InvalidVariableName {
            login: bad.login.id,
        });
    }

    let mut by_name: BTreeMap<String, Vec<Found<T>>> = BTreeMap::new();
    for f in live {
        by_name.entry(f.variable.clone()).or_default().push(f);
    }
    Ok(by_name
        .into_values()
        .filter_map(|mut group| {
            let candidates = group.len();
            group.sort_by(|a, b| get::ranking((&a.login, a.tier), (&b.login, b.tier)));
            group
                .into_iter()
                .next()
                .map(|found| Chosen { found, candidates })
        })
        .collect())
}

/// `[A-Za-z_][A-Za-z0-9_]*` — a name every shell and every OS accepts.
fn is_variable_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use heyl_domain::LoginId;

    const PREFIX: &str = "github.com/mrh1997/heyl/prod#";

    fn login(id: u8, name: &str, edited: &str) -> Login {
        Login {
            id: LoginId::parse(&format!("00000000-0000-4000-8000-0000000000{id:02x}"))
                .expect("valid uuid"),
            display_headline: Some(name.to_owned()),
            title: String::new(),
            username: String::new(),
            note: String::new(),
            websites: Vec::new(),
            tags: Vec::new(),
            password: None,
            custom_fields: Vec::new(),
            created: None,
            edited: Some(edited.to_owned()),
            change_time: None,
            is_deleted: false,
            is_archived: false,
        }
    }

    fn found(login: Login, tier: u8, payload: &'static str) -> Option<Found<&'static str>> {
        let variable = variable_name(&login, PREFIX)?.to_owned();
        Some(Found {
            variable,
            tier,
            login,
            payload,
        })
    }

    /// The error, where success was wrong. `Chosen` is not `Debug` — it holds
    /// what decrypts a secret — so `expect_err` is not available.
    fn refused<T>(result: Result<T, AppError>) -> AppError {
        match result {
            Err(e) => e,
            Ok(_) => panic!("expected a refusal"),
        }
    }

    fn names<T>(chosen: &[Chosen<T>]) -> Vec<&str> {
        chosen.iter().map(|c| c.found.variable.as_str()).collect()
    }

    #[test]
    fn a_project_url_gets_a_hash_appended() {
        let p = Project::parse("github.com/mrh1997/heyl/prod").expect("valid");
        assert_eq!(p.prefix(), PREFIX);
    }

    #[test]
    fn a_project_url_is_validated() {
        for bad in [
            "",
            "https://github.com/x",
            "heyl://github.com/x",
            "github.com/x/",
            "github.com/x#y",
        ] {
            let e = Project::parse(bad).expect_err(bad);
            assert!(matches!(e, AppError::BadProject { .. }), "{bad}");
            assert_eq!(e.exit_code() as u8, 7, "{bad}");
        }
    }

    #[test]
    fn the_prefix_must_be_followed_by_the_hash() {
        let l = |name| login(1, name, "2026-01-01");
        assert_eq!(
            variable_name(&l("github.com/mrh1997/heyl/prod#DB"), PREFIX),
            Some("DB")
        );
        // Another environment, the project as a whole, a longer environment
        // name and a different case all miss.
        for miss in [
            "github.com/mrh1997/heyl/stage#DB",
            "github.com/mrh1997/heyl#DB",
            "github.com/mrh1997/heyl/production#DB",
            "GitHub.com/mrh1997/heyl/prod#DB",
            "heyl://github.com/mrh1997/heyl/prod#DB",
            "github.com",
        ] {
            assert_eq!(variable_name(&l(miss), PREFIX), None, "{miss}");
        }
    }

    #[test]
    fn the_title_counts_when_the_display_name_does_not_match() {
        let mut l = login(1, "GitHub prod DB", "2026-01-01");
        l.title = format!("{PREFIX}DATABASE_URL");
        assert_eq!(variable_name(&l, PREFIX), Some("DATABASE_URL"));
    }

    #[test]
    fn a_website_counts_with_or_without_a_scheme() {
        // As the heylogin app stores one typed into the website field: no
        // name, no title.
        let mut l = login(1, "", "2026-01-01");
        l.display_headline = None;
        l.websites = vec![format!("{PREFIX}TEST")];
        assert_eq!(variable_name(&l, PREFIX), Some("TEST"));

        l.websites = vec!["example.org".to_owned(), format!("https://{PREFIX}TOKEN")];
        assert_eq!(variable_name(&l, PREFIX), Some("TOKEN"));

        // Only the two web schemes are tolerated.
        l.websites = vec![format!("heyl://{PREFIX}TOKEN")];
        assert_eq!(variable_name(&l, PREFIX), None);
    }

    #[test]
    fn every_live_match_becomes_a_variable_sorted_by_name() {
        let chosen = choose(
            [
                found(
                    login(1, &format!("{PREFIX}STRIPE_KEY"), "2026-01-01"),
                    0,
                    "a",
                ),
                found(
                    login(2, &format!("{PREFIX}DATABASE_URL"), "2026-01-01"),
                    0,
                    "b",
                ),
            ]
            .into_iter()
            .flatten()
            .collect(),
        )
        .expect("two variables");
        assert_eq!(names(&chosen), ["DATABASE_URL", "STRIPE_KEY"]);
        assert!(chosen.iter().all(|c| c.candidates == 1));
    }

    #[test]
    fn archived_and_deleted_logins_are_skipped() {
        let mut archived = login(1, &format!("{PREFIX}OLD"), "2026-01-01");
        archived.is_deleted = true;
        archived.is_archived = true;
        let mut deleted = login(2, &format!("{PREFIX}GONE"), "2026-01-01");
        deleted.is_deleted = true;
        let live = login(3, &format!("{PREFIX}KEPT"), "2026-01-01");

        let chosen = choose(
            [
                found(archived, 0, "a"),
                found(deleted, 0, "b"),
                found(live, 0, "c"),
            ]
            .into_iter()
            .flatten()
            .collect(),
        )
        .expect("one live variable");
        assert_eq!(names(&chosen), ["KEPT"]);
    }

    #[test]
    fn no_live_match_is_not_found() {
        let e = refused(choose::<&str>(Vec::new()));
        assert!(matches!(e, AppError::NoProjectSecrets));
        assert_eq!(e.exit_code() as u8, 2);
    }

    #[test]
    fn a_name_that_is_not_a_variable_fails_the_whole_run() {
        for bad in ["", "1ST", "MY-VAR", "A B", "X=Y", "ÄRGER"] {
            let e = refused(choose(
                [found(
                    login(7, &format!("{PREFIX}{bad}"), "2026-01-01"),
                    0,
                    "a",
                )]
                .into_iter()
                .flatten()
                .collect(),
            ));
            assert!(matches!(e, AppError::InvalidVariableName { .. }), "{bad:?}");
            assert_eq!(e.exit_code() as u8, 7, "{bad:?}");
        }
        for good in ["A", "_", "_X1", "database_url", "Path"] {
            assert!(is_variable_name(good), "{good}");
        }
    }

    #[test]
    fn duplicates_rank_private_first_then_most_recent() {
        let name = format!("{PREFIX}TOKEN");
        let chosen = choose(
            [
                // A newer team entry does not beat a private one…
                found(
                    login(1, &name, "2026-09-01"),
                    get::tier(VaultType::Team),
                    "team",
                ),
                found(
                    login(2, &name, "2026-01-01"),
                    get::tier(VaultType::Private),
                    "old",
                ),
                // …but among private ones the most recent wins.
                found(
                    login(3, &name, "2026-05-01"),
                    get::tier(VaultType::Private),
                    "new",
                ),
            ]
            .into_iter()
            .flatten()
            .collect(),
        )
        .expect("one variable");
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].found.payload, "new");
        assert_eq!(chosen[0].candidates, 3);
    }
}
