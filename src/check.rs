//! HTTP link checking (SPEC §5–§6).
//!
//! A `reqwest` blocking client performs each check with bounded synchronous
//! concurrency (a fixed worker pool). HEAD is attempted first; on a `405`,
//! `400` or `501` response the request is retried once with GET and the body
//! discarded. Redirects are followed up to 10 hops by default (recording the
//! chain); with `--no-follow-redirects` a 3xx is reported as an *unfollowed*
//! redirect.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use reqwest::blocking::Client;
use reqwest::redirect::Policy;
use reqwest::StatusCode;

/// A single redirect hop in the recorded chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RedirectHop {
    pub status: u16,
    pub url: String,
}

/// High-level outcome of one checked URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Ok,
    Broken,
    Redirect,
    Error,
}

/// Result of checking a single URL.
#[derive(Clone, Debug)]
pub struct CheckResult {
    pub url: String,
    pub status: Status,
    pub http_status: Option<u16>,
    pub redirect_chain: Vec<RedirectHop>,
    /// True when a 3xx was left *unfollowed* (`--no-follow-redirects`); this is
    /// a reportable finding for exit-code purposes.
    pub redirect_unfollowed: bool,
    pub error: Option<String>,
}

/// Options shared by every check in a run.
#[derive(Clone, Debug)]
pub struct CheckOptions {
    pub timeout_secs: u64,
    pub concurrency: usize,
    pub user_agent: String,
    pub follow_redirects: bool,
}

// reqwest's redirect policy runs on the calling worker thread, so a
// thread-local captures the chain for the in-flight request without cross-talk.
thread_local! {
    static CHAIN: std::cell::RefCell<Vec<RedirectHop>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Build the blocking client for a run.
fn build_client(options: &CheckOptions) -> Result<Client, String> {
    let mut builder = Client::builder()
        .timeout(Duration::from_secs(options.timeout_secs))
        .connect_timeout(Duration::from_secs(5))
        .user_agent(&options.user_agent);
    if options.follow_redirects {
        // Custom policy records the chain while following up to 10 hops.
        builder = builder.redirect(Policy::custom(|attempt| {
            // Each invocation is one redirect: record `status` -> destination.
            CHAIN.with(|c| {
                c.borrow_mut().push(RedirectHop {
                    status: attempt.status().as_u16(),
                    url: attempt.url().to_string(),
                });
            });
            if attempt.previous().len() >= 10 {
                attempt.error("too many redirects")
            } else {
                attempt.follow()
            }
        }));
    } else {
        builder = builder.redirect(Policy::none());
    }
    builder.build().map_err(|e| e.to_string())
}

fn take_chain() -> Vec<RedirectHop> {
    CHAIN.with(|c| std::mem::take(&mut *c.borrow_mut()))
}

/// Perform one HTTP check, returning a typed result (never panics).
fn check_one(client: &Client, url: &str, follow: bool) -> CheckResult {
    let head = match client.head(url).send() {
        Ok(r) => r,
        Err(e) => return transport_error(url, e.to_string()),
    };
    let mut chain = take_chain();
    let mut status = head.status();

    // HEAD may be disallowed: retry once with GET and discard the body.
    if matches!(
        status,
        StatusCode::METHOD_NOT_ALLOWED | StatusCode::BAD_REQUEST | StatusCode::NOT_IMPLEMENTED
    ) {
        CHAIN.with(|c| c.borrow_mut().clear());
        match client.get(url).send() {
            Ok(r) => {
                chain.extend(take_chain());
                status = r.status();
            }
            Err(e) => return transport_error(url, e.to_string()),
        }
    }

    classify(url, status, chain, follow)
}

/// Map an HTTP status + redirect chain into a typed result (SPEC §6).
fn classify(url: &str, status: StatusCode, chain: Vec<RedirectHop>, follow: bool) -> CheckResult {
    let code = status.as_u16();
    let (status_kind, unfollowed) = if status.is_success() {
        // Followed a redirect that ended 2xx -> reported REDIRECT but healthy.
        if chain.is_empty() {
            (Status::Ok, false)
        } else {
            (Status::Redirect, false)
        }
    } else if status.is_redirection() {
        // Reached while following, or unfollowed by request.
        (Status::Redirect, !follow)
    } else if status.is_client_error() || status.is_server_error() {
        (Status::Broken, false)
    } else {
        (Status::Error, false)
    };
    CheckResult {
        url: url.to_string(),
        status: status_kind,
        http_status: Some(code),
        redirect_chain: chain,
        redirect_unfollowed: unfollowed,
        error: None,
    }
}

fn transport_error(url: &str, msg: String) -> CheckResult {
    CheckResult {
        url: url.to_string(),
        status: Status::Error,
        http_status: None,
        redirect_chain: Vec::new(),
        redirect_unfollowed: false,
        error: Some(msg),
    }
}

/// Check all candidate URLs with at most `concurrency` outstanding requests.
///
/// When `fail_fast` is set, the first BROKEN (or unfollowed-redirect) finding
/// cancels any remaining queued work; in-flight requests finish (bounded by the
/// per-request timeout). Results are returned in arbitrary order; the caller
/// sorts for deterministic output.
pub fn check_all(urls: &[String], options: &CheckOptions, fail_fast: bool) -> Vec<CheckResult> {
    if urls.is_empty() {
        return Vec::new();
    }
    let Ok(client) = build_client(options) else {
        // A client build failure is a hard error surfaced per URL so nothing
        // panics and every result stays typed.
        return urls
            .iter()
            .map(|u| transport_error(u, "failed to initialise HTTP client".to_string()))
            .collect();
    };

    let queue: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(urls.iter().cloned().collect()));
    let results: Arc<Mutex<Vec<CheckResult>>> = Arc::new(Mutex::new(Vec::new()));
    let cancel = Arc::new(AtomicBool::new(false));

    let n = options.concurrency.max(1);
    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(n);
        for sid in 0..n {
            let queue = Arc::clone(&queue);
            let results = Arc::clone(&results);
            let cancel = Arc::clone(&cancel);
            let follow = options.follow_redirects;
            let client = client.clone();
            handles.push(scope.spawn(move || {
                let _ = sid;
                loop {
                    if cancel.load(Ordering::Relaxed) && fail_fast {
                        break;
                    }
                    let url = { lock(&queue).pop_front() };
                    let Some(url) = url else { break };
                    let res = check_one(&client, &url, follow);
                    let reportable = matches!(res.status, Status::Broken)
                        || (matches!(res.status, Status::Redirect) && res.redirect_unfollowed);
                    if fail_fast && reportable {
                        cancel.store(true, Ordering::Relaxed);
                    }
                    lock(&results).push(res);
                }
            }));
        }
        for h in handles {
            let _ = h.join();
        }
    });

    let out = lock(&results).drain(..).collect();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(s: u16, chain: Vec<RedirectHop>, follow: bool) -> CheckResult {
        super::classify(
            "https://x.example",
            StatusCode::from_u16(s).expect("valid status"),
            chain,
            follow,
        )
    }

    #[test]
    fn ok_2xx() {
        let r = classify(200, vec![], true);
        assert_eq!(r.status, Status::Ok);
        assert_eq!(r.http_status, Some(200));
        assert!(!r.redirect_unfollowed);
    }

    #[test]
    fn followed_redirect_ending_2xx_is_redirect_but_healthy() {
        let chain = vec![RedirectHop {
            status: 301,
            url: "https://new.example".into(),
        }];
        let r = classify(200, chain, true);
        assert_eq!(r.status, Status::Redirect);
        assert!(!r.redirect_unfollowed);
    }

    #[test]
    fn unfollowed_3xx_is_reportable() {
        let r = classify(301, vec![], false);
        assert_eq!(r.status, Status::Redirect);
        assert!(r.redirect_unfollowed);
    }

    #[test]
    fn followed_3xx_without_final_success_is_redirect() {
        let r = classify(302, vec![], true);
        assert_eq!(r.status, Status::Redirect);
        assert!(!r.redirect_unfollowed);
    }

    #[test]
    fn broken_4xx_and_5xx() {
        let r4 = classify(404, vec![], true);
        assert_eq!(r4.status, Status::Broken);
        let r5 = classify(503, vec![], true);
        assert_eq!(r5.status, Status::Broken);
    }

    #[test]
    fn invalid_status_is_error() {
        let r = classify(100, vec![], true);
        assert_eq!(r.status, Status::Error);
    }
}
