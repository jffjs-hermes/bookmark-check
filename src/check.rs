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

/// Maximum redirects followed per URL (SPEC §6, browser behaviour).
const MAX_REDIRECTS: usize = 10;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Build the blocking client for a run. Redirects are *not* handled by
/// reqwest's own policy: we follow them manually in `check_one` so the chain
/// is captured deterministically on this thread (reqwest's redirect closure
/// runs on an internal runtime thread, so it cannot write a caller-owned
/// buffer). `Policy::none` guarantees the response we see is the first hop.
fn build_client(options: &CheckOptions) -> Result<Client, String> {
    Client::builder()
        .timeout(Duration::from_secs(options.timeout_secs))
        .connect_timeout(Duration::from_secs(5))
        .user_agent(&options.user_agent)
        .redirect(Policy::none())
        .build()
        .map_err(|e| e.to_string())
}

/// One request against `url`: HEAD first, retrying once with GET when the
/// server rejects HEAD (405/400/501). Returns the response status and, for a
/// 3xx, the resolved `Location` destination. Never panics; a transport error
/// is returned as `Err(message)` per URL.
fn request_once(client: &Client, url: &str) -> Result<(StatusCode, Option<String>), String> {
    let (status, location) = match client.head(url).send() {
        Ok(r) => {
            let s = r.status();
            let loc = location_of(&r);
            if matches!(
                s,
                StatusCode::METHOD_NOT_ALLOWED
                    | StatusCode::BAD_REQUEST
                    | StatusCode::NOT_IMPLEMENTED
            ) {
                // HEAD disallowed: retry once with GET and discard the body.
                match client.get(url).send() {
                    Ok(g) => (g.status(), location_of(&g)),
                    Err(e) => return Err(e.to_string()),
                }
            } else {
                (s, loc)
            }
        }
        Err(e) => return Err(e.to_string()),
    };
    Ok((status, location))
}

fn location_of(resp: &reqwest::blocking::Response) -> Option<String> {
    resp.headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// Resolve a possibly-relative `Location` against `base`. Returns a transport-
/// style error message when it cannot be parsed.
fn resolve_location(base: &str, location: &str) -> Result<String, String> {
    match url::Url::parse(base).and_then(|u| u.join(location)) {
        Ok(joined) => Ok(joined.to_string()),
        Err(e) => Err(format!("invalid redirect location {location:?}: {e}")),
    }
}

/// Perform one HTTP check, returning a typed result (never panics).
///
/// Redirects are followed manually (up to `MAX_REDIRECTS` hops) so the chain
/// is captured in order on this thread. With `follow == false` the first 3xx
/// is reported as an *unfollowed* redirect. A response past the hop cap yields
/// a redirect finding rather than a transport error (the caller chose to
/// follow and we record it as far as we can).
fn check_one(client: &Client, url: &str, follow: bool) -> CheckResult {
    let mut current = url.to_string();
    let mut chain: Vec<RedirectHop> = Vec::new();

    for _ in 0..=MAX_REDIRECTS {
        let (status, location) = match request_once(client, &current) {
            Ok(x) => x,
            Err(e) => return transport_error(url, e),
        };

        if status.is_redirection() {
            if follow && chain.len() < MAX_REDIRECTS {
                // Follow this hop only if a resolvable Location is present.
                let dest = match location {
                    Some(loc) => match resolve_location(&current, &loc) {
                        Ok(d) => d,
                        Err(e) => return transport_error(url, e),
                    },
                    // 3xx without Location: nothing to follow; report the
                    // redirect as found (classify sets unfollowed=true when
                    // follow is off).
                    None => return classify(url, status, chain, follow),
                };
                chain.push(RedirectHop {
                    status: status.as_u16(),
                    url: dest.clone(),
                });
                current = dest;
                continue;
            }
            // Not following, or at the hop cap: report the redirect as found.
            return classify(url, status, chain, follow);
        }

        return classify(url, status, chain, follow);
    }

    unreachable!("loop bounded by MAX_REDIRECTS")
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

    #[test]
    fn transport_error_is_typed_without_http_status() {
        let r = transport_error("https://x.example", "connection refused".to_string());
        assert_eq!(r.status, Status::Error);
        assert_eq!(r.http_status, None);
        assert!(r.redirect_chain.is_empty());
        assert!(!r.redirect_unfollowed);
        assert_eq!(r.error.as_deref(), Some("connection refused"));
    }

    #[test]
    fn redirect_chain_order_is_preserved() {
        let chain = vec![
            RedirectHop {
                status: 301,
                url: "https://hop1.example".into(),
            },
            RedirectHop {
                status: 302,
                url: "https://hop2.example".into(),
            },
        ];
        let r = classify(200, chain, true);
        assert_eq!(r.status, Status::Redirect);
        assert!(!r.redirect_unfollowed);
        assert_eq!(r.redirect_chain.len(), 2);
        assert_eq!(r.redirect_chain[0].status, 301);
        assert_eq!(r.redirect_chain[1].status, 302);
    }

    #[test]
    fn redirect_unfollowed_flag_is_set_only_when_not_following() {
        let unfollowed = classify(301, vec![], false);
        assert!(unfollowed.redirect_unfollowed);
        let followed = classify(301, vec![], true);
        assert!(!followed.redirect_unfollowed);
    }
}
