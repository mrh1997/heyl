//! Telling a person that their phone is waiting for them.
//!
//! An unlock or a pairing blocks on a human, and the human may not be looking
//! at the terminal heyl runs in — a script or an agent started it. This port
//! puts the request somewhere they will see it: a window, or a command of
//! their choosing (DESIGN.md §5). The terminal note is printed regardless; this
//! is in addition to it, never instead.

use crate::terminal::QrStyle;

/// What the person is asked to do.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum ApprovalRequest<'a> {
    /// Approve an unlock request in the heylogin app.
    Unlock {
        /// The session slot asking.
        session: &'a str,
        /// Who asks, by the name the phone shows.
        heading: &'a str,
        /// What to do about it.
        message: &'a str,
    },
    /// Scan a pairing code with the heylogin app.
    Pair {
        /// The session slot being created.
        session: &'a str,
        /// The pairing URL the code encodes.
        url: &'a str,
        /// How the terminal would draw it, for an adapter that falls back.
        style: QrStyle,
    },
}

/// How the wait ended, which the prompt is told as it closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApprovalOutcome {
    /// The phone approved.
    Approved,
    /// The wait gave up (`--wait`).
    Timeout,
    /// Anything else ended it — an error, or heyl's own cancellation.
    Failed,
}

impl ApprovalOutcome {
    /// The word the prompt receives as its final line.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::Timeout => "timeout",
            Self::Failed => "failed",
        }
    }
}

/// Shows approval requests.
#[async_trait::async_trait]
pub trait ApprovalUi: Send + Sync {
    /// Put the request in front of the person.
    ///
    /// Infallible by design: a window that cannot open is not a reason to
    /// fail a command whose terminal note is already on stderr. The returned
    /// prompt says whether anything actually appeared.
    async fn show(&self, request: ApprovalRequest<'_>) -> Box<dyn ApprovalPrompt>;
}

/// One request on screen.
#[async_trait::async_trait]
pub trait ApprovalPrompt: Send + Sync {
    /// Whether the request is known to be on screen.
    ///
    /// Only a pairing waits to find out, because only a pairing has something
    /// to draw in the terminal instead — the code.
    fn is_shown(&self) -> bool;

    /// Resolves when the person cancelled. Never resolves otherwise.
    async fn cancelled(&self);

    /// Take the request off screen, saying how the wait ended.
    async fn close(self: Box<Self>, outcome: ApprovalOutcome);
}
