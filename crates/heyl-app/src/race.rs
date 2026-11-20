//! Racing a wait against the person pressing Cancel.
//!
//! `heyl-app` has no runtime, so no `select!`: two futures polled in turn is
//! all a race needs.

use std::future::{Future, poll_fn};
use std::pin::pin;
use std::task::Poll;

use crate::AppError;

/// Run `work` until it finishes, or until `cancel` resolves first.
///
/// `work` is dropped mid-flight on a cancel — a long-poll or a `Sync` loop,
/// neither of which leaves anything half-done behind.
///
/// # Errors
/// Whatever `work` fails with, or [`AppError::Cancelled`].
pub async fn until_cancelled<T>(
    work: impl Future<Output = Result<T, AppError>>,
    cancel: impl Future<Output = ()>,
) -> Result<T, AppError> {
    let mut work = pin!(work);
    let mut cancel = pin!(cancel);
    poll_fn(|cx| {
        if let Poll::Ready(result) = work.as_mut().poll(cx) {
            return Poll::Ready(result);
        }
        if cancel.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(AppError::Cancelled));
        }
        Poll::Pending
    })
    .await
}
