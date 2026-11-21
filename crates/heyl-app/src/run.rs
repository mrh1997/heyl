//! `heyl run` — a project's secrets, from one login's custom fields.
//!
//! There is no template file. A project is one login: `--project` is its URL,
//! e.g. `github.com/mrh1997/heyl/prod`, matched against the login's name, title
//! or websites the way `get -l` matches (exact, case-insensitive), with a
//! leading `https://` or `http://` on either side ignored. Each of that
//! login's **custom fields** becomes one environment variable — the field's
//! name is the variable, its value the secret. DESIGN.md §5 (`run`) holds
//! every rule; this module resolves the variables, and `heyl-cli` spawns the
//! child with them.
//!
//! Secrets leave as [`Zeroizing`] strings: the CLI copies them into the child's
//! environment and drops them, which is as far as a process can carry them.

use std::collections::BTreeSet;

use heyl_domain::{LoginId, VaultType};
use heyl_vault::login::{FieldValue, Liveness, Login};
use zeroize::Zeroizing;

use crate::get::{self, Candidate, VaultSelector};
use crate::{AppError, Ports};

/// A validated `--project` value: the project login's URL.
#[derive(Debug, Clone)]
pub struct Project {
    /// The URL as typed, for messages.
    url: String,
    /// Scheme-stripped and lower-cased, for matching.
    key: String,
}

impl Project {
    /// Validate a `--project` value.
    ///
    /// # Errors
    /// [`AppError::BadProject`] (exit 7) if it is empty, or nothing but a scheme.
    pub fn parse(project: &str) -> Result<Self, AppError> {
        let key = match_key(project.trim());
        if key.is_empty() {
            return Err(AppError::BadProject {
                reason: "it is empty",
            });
        }
        Ok(Self {
            url: project.to_owned(),
            key,
        })
    }

    /// The URL as typed.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Whether a login is this project's: its name, title or one of its
    /// websites equals the project URL, ignoring case and a leading
    /// `https://`/`http://` on either side.
    #[must_use]
    pub fn matches(&self, login: &Login) -> bool {
        login
            .display_headline
            .as_deref()
            .into_iter()
            .chain(std::iter::once(login.title.as_str()))
            .chain(login.websites.iter().map(String::as_str))
            .any(|name| match_key(name) == self.key)
    }
}

/// A URL reduced to what matching compares: no web scheme, lower case.
fn match_key(url: &str) -> String {
    let lower = url.to_lowercase();
    lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
        .unwrap_or(&lower)
        .to_owned()
}

/// One environment variable for the child.
pub struct Variable {
    /// Its name — the custom field's name.
    pub name: String,
    /// The custom field's value, decrypted.
    pub value: Zeroizing<String>,
}

impl std::fmt::Debug for Variable {
    // Hand-written so a value can never reach a log or a panic message.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Variable")
            .field("name", &self.name)
            .field("value", &"<redacted>")
            .finish()
    }
}

/// What `run` resolved, for the CLI to report on and inject.
#[derive(Debug)]
pub struct Resolved {
    /// The login whose custom fields were used.
    pub login: LoginId,
    /// How many live logins matched the project; above 1, the ranking chose.
    pub match_count: usize,
    /// The variables, sorted by name. Empty if the login has no custom fields.
    pub variables: Vec<Variable>,
}

/// Resolve a project's variables.
///
/// Unlocks like every read, asking the phone and waiting if needed (bounded by
/// `wait`).
///
/// # Errors
/// [`AppError::NoProjectSecrets`] (exit 2) when no live login matches;
/// [`AppError::InvalidVariableName`] / [`AppError::DuplicateVariable`] (exit 7)
/// when a custom field cannot become a variable; [`AppError::NoSuchVault`] and
/// the vault-open errors for a named vault; the usual unlock and backend errors.
pub async fn run(
    ports: &Ports<'_>,
    slot: &crate::session::Slot,
    project: &Project,
    vault: &VaultSelector,
    wait: Option<u64>,
) -> Result<Resolved, AppError> {
    let session = crate::unlock::ensure(ports, slot, wait).await?;
    let scope = get::resolve_scope(ports, &session, vault).await?;
    let named = !matches!(vault, VaultSelector::Any);

    let mut candidates: Vec<Candidate> = Vec::new();
    for summary in scope {
        // An inbox holds what other people sent you, and a field's name
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
            // Only live logins: one you deleted must not keep injecting
            // itself into builds.
            if login.liveness() == Liveness::Live && project.matches(&login) {
                candidates.push(Candidate {
                    tier: get::tier(summary.vault_type),
                    login,
                    protected: opened.protected.clone(),
                });
            }
        }
    }

    let match_count = candidates.len();
    let chosen = get::rank(candidates).ok_or(AppError::NoProjectSecrets)?;
    let variables = variables(&chosen.login, |value| match value {
        FieldValue::Plain(s) => Ok(s.clone()),
        FieldValue::Protected(p) => get::decrypt(&chosen.protected, p),
    })?;
    Ok(Resolved {
        login: chosen.login.id,
        match_count,
        variables,
    })
}

/// Turn a login's custom fields into variables, sorted by name.
///
/// Every name is checked before anything is decrypted. Generic over what
/// decrypts a value, so the rules are testable without a key.
fn variables(
    login: &Login,
    mut decrypt: impl FnMut(&FieldValue) -> Result<String, AppError>,
) -> Result<Vec<Variable>, AppError> {
    let mut seen = BTreeSet::new();
    for field in &login.custom_fields {
        if !is_variable_name(&field.name) {
            return Err(AppError::InvalidVariableName {
                login: login.id,
                name: field.name.clone(),
            });
        }
        if !seen.insert(field.name.as_str()) {
            return Err(AppError::DuplicateVariable {
                login: login.id,
                name: field.name.clone(),
            });
        }
    }

    let mut variables = Vec::with_capacity(login.custom_fields.len());
    for field in &login.custom_fields {
        variables.push(Variable {
            name: field.name.clone(),
            value: Zeroizing::new(decrypt(&field.value)?),
        });
    }
    variables.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(variables)
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
    use heyl_vault::login::CustomField;

    fn login(name: &str) -> Login {
        Login {
            id: LoginId::parse("00000000-0000-4000-8000-000000000001").expect("valid uuid"),
            display_headline: Some(name.to_owned()),
            title: String::new(),
            username: String::new(),
            note: String::new(),
            websites: Vec::new(),
            tags: Vec::new(),
            password: None,
            custom_fields: Vec::new(),
            created: None,
            edited: Some("2026-01-01".to_owned()),
            change_time: None,
            is_deleted: false,
            is_archived: false,
        }
    }

    fn with_fields(fields: &[(&str, &str)]) -> Login {
        let mut l = login("github.com/me/app");
        l.custom_fields = fields
            .iter()
            .map(|(name, value)| CustomField {
                id: None,
                name: (*name).to_owned(),
                value: FieldValue::Plain((*value).to_owned()),
            })
            .collect();
        l
    }

    #[allow(clippy::unnecessary_wraps)] // the shape `variables` takes
    fn plain(value: &FieldValue) -> Result<String, AppError> {
        match value {
            FieldValue::Plain(s) => Ok(s.clone()),
            FieldValue::Protected(_) => panic!("tests use plain values"),
        }
    }

    fn project(url: &str) -> Project {
        Project::parse(url).expect("valid")
    }

    #[test]
    fn only_an_empty_project_is_invalid() {
        for bad in ["", "  ", "https://", "http://"] {
            let e = Project::parse(bad).expect_err(bad);
            assert!(matches!(e, AppError::BadProject { .. }), "{bad:?}");
            assert_eq!(e.exit_code() as u8, 7, "{bad:?}");
        }
        for good in ["github.com/me/app", "https://github.com/me/app/", "a#b"] {
            assert!(Project::parse(good).is_ok(), "{good}");
        }
    }

    #[test]
    fn the_whole_url_must_match_ignoring_case_and_scheme() {
        let p = project("github.com/me/app/prod");
        for hit in [
            "github.com/me/app/prod",
            "GitHub.com/me/App/prod",
            "https://github.com/me/app/prod",
            "http://github.com/me/app/prod",
        ] {
            assert!(p.matches(&login(hit)), "{hit}");
        }
        for miss in [
            "github.com/me/app",
            "github.com/me/app/production",
            "github.com/me/app/prod#DB",
            "heyl://github.com/me/app/prod",
            "github.com",
        ] {
            assert!(!p.matches(&login(miss)), "{miss}");
        }
        // A scheme on the project is ignored, too.
        assert!(
            project("https://github.com/me/app/prod").matches(&login("github.com/me/app/prod"))
        );
    }

    #[test]
    fn title_and_websites_count() {
        let p = project("github.com/me/app");
        let mut l = login("My app");
        l.title = "github.com/me/app".to_owned();
        assert!(p.matches(&l));

        // As the heylogin app stores one typed into the website field.
        let mut l = login("");
        l.display_headline = None;
        l.websites = vec![
            "example.org".to_owned(),
            "https://github.com/me/app".to_owned(),
        ];
        assert!(p.matches(&l));
    }

    #[test]
    fn custom_fields_become_variables_sorted_by_name() {
        let vars = variables(
            &with_fields(&[("STRIPE_KEY", "s"), ("DATABASE_URL", "d"), ("EMPTY", "")]),
            plain,
        )
        .expect("valid");
        let got: Vec<(&str, &str)> = vars
            .iter()
            .map(|v| (v.name.as_str(), v.value.as_str()))
            .collect();
        assert_eq!(
            got,
            [("DATABASE_URL", "d"), ("EMPTY", ""), ("STRIPE_KEY", "s")]
        );
    }

    #[test]
    fn no_custom_fields_is_no_variables() {
        let vars = variables(&with_fields(&[]), plain).expect("valid");
        assert!(vars.is_empty());
    }

    #[test]
    fn a_field_name_that_is_not_a_variable_fails_the_whole_run() {
        for bad in ["", "1ST", "MY-VAR", "A B", "X=Y", "ÄRGER"] {
            let e = variables(&with_fields(&[("OK", "1"), (bad, "2")]), plain).expect_err(bad);
            assert!(
                matches!(&e, AppError::InvalidVariableName { name, .. } if name == bad),
                "{bad:?}"
            );
            assert_eq!(e.exit_code() as u8, 7, "{bad:?}");
        }
        for good in ["A", "_", "_X1", "database_url", "Path"] {
            assert!(is_variable_name(good), "{good}");
        }
    }

    #[test]
    fn a_repeated_field_name_fails_the_whole_run() {
        let e = variables(&with_fields(&[("TOKEN", "a"), ("TOKEN", "b")]), plain)
            .expect_err("duplicate");
        assert!(matches!(&e, AppError::DuplicateVariable { name, .. } if name == "TOKEN"));
        assert_eq!(e.exit_code() as u8, 7);
    }

    #[test]
    fn names_are_checked_before_anything_is_decrypted() {
        let mut decrypted = 0;
        let e = variables(&with_fields(&[("GOOD", "a"), ("bad-name", "b")]), |v| {
            decrypted += 1;
            plain(v)
        });
        assert!(e.is_err());
        assert_eq!(decrypted, 0);
    }

    #[test]
    fn no_match_is_not_found() {
        assert_eq!(AppError::NoProjectSecrets.exit_code() as u8, 2);
    }
}
