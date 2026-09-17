//! Report model and CLI output (SPEC §3, §7–§8).

use crate::check::{CheckResult, Status};
use crate::extract::{Location, Malformed, Occurrence};

/// One reportable occurrence, tied to its check result (or malformed).
#[derive(Debug)]
pub struct Entry {
    pub url: String,
    pub location: Location,
    pub kind: Kind,
    pub http_status: Option<u16>,
    pub redirect_chain: Vec<crate::check::RedirectHop>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Ok,
    Broken,
    Redirect,
    Error,
    Malformed,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Ok => "ok",
            Kind::Broken => "broken",
            Kind::Redirect => "redirect",
            Kind::Error => "error",
            Kind::Malformed => "malformed",
        }
    }
}

#[derive(Debug, Default)]
pub struct Summary {
    pub total: usize,
    pub ok: usize,
    pub broken: usize,
    pub redirects: usize,
    pub errors: usize,
    pub malformed: usize,
}

/// Build the sorted entry list given extracted occurrences, malformed links
/// and the collected check results.
///
/// Output is deterministic: entries are ordered by `(file, line, column)`.
pub fn build_entries(
    occurrences: &[Occurrence],
    malformed: &[Malformed],
    by_url: &std::collections::HashMap<String, CheckResult>,
) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();

    for occ in occurrences {
        let res = by_url.get(&occ.url);
        let entry = match res {
            Some(r) => Entry {
                url: occ.url.clone(),
                location: occ.location.clone(),
                kind: match r.status {
                    Status::Ok => Kind::Ok,
                    Status::Broken => Kind::Broken,
                    Status::Redirect => Kind::Redirect,
                    Status::Error => Kind::Error,
                },
                http_status: r.http_status,
                redirect_chain: r.redirect_chain.clone(),
                error: r.error.clone(),
            },
            // A URL that was never checked (e.g. skipped by fail-fast) is
            // reported as an error so it is not silently dropped.
            None => Entry {
                url: occ.url.clone(),
                location: occ.location.clone(),
                kind: Kind::Error,
                http_status: None,
                redirect_chain: Vec::new(),
                error: Some("skipped (fail-fast)".to_string()),
            },
        };
        entries.push(entry);
    }

    for m in malformed {
        entries.push(Entry {
            url: m.url.clone(),
            location: m.location.clone(),
            kind: Kind::Malformed,
            http_status: None,
            redirect_chain: Vec::new(),
            // Carry the specific problem label for text rendering.
            error: Some(problem_label(&m.problem).to_string()),
        });
    }

    entries.sort_by(|a, b| {
        (&a.location.file, a.location.line, a.location.column).cmp(&(
            &b.location.file,
            b.location.line,
            b.location.column,
        ))
    });
    entries
}

pub fn summarize(entries: &[Entry]) -> Summary {
    let mut s = Summary {
        total: entries.len(),
        ..Summary::default()
    };
    for e in entries {
        match e.kind {
            Kind::Ok => s.ok += 1,
            Kind::Broken => s.broken += 1,
            Kind::Redirect => s.redirects += 1,
            Kind::Error => s.errors += 1,
            Kind::Malformed => s.malformed += 1,
        }
    }
    s
}

/// Exit code per SPEC §3: 0 healthy, 1 broken/unfollowed-redirect, 2 usage.
pub fn exit_code(entries: &[Entry]) -> i32 {
    for e in entries {
        match e.kind {
            Kind::Broken => return 1,
            Kind::Redirect if http_is_3xx(e.http_status) && is_unfollowed(e) => return 1,
            _ => {}
        }
    }
    0
}

fn http_is_3xx(status: Option<u16>) -> bool {
    matches!(status, Some(s) if (300..400).contains(&s))
}

// A redirect is "unfollowed" when the top-level result carried the flag. We
// re-derive it from the entry by checking whether a redirect chain is empty but
// the status is 3xx (no-follow case), since the flag lives on CheckResult.
fn is_unfollowed(e: &Entry) -> bool {
    e.redirect_chain.is_empty() && http_is_3xx(e.http_status)
}

fn problem_label(m: &crate::extract::Problem) -> &'static str {
    use crate::extract::Problem;
    match m {
        Problem::EmptyUrl => "empty url",
        Problem::InvalidUrl => "invalid url",
        Problem::MissingHost => "missing host",
        Problem::UnmatchedBrackets => "unmatched brackets",
        Problem::UnbalancedTitle => "unbalanced title",
    }
}

/// Human-readable text output (SPEC §3), one line per entry.
pub fn render_text(entries: &[Entry]) -> String {
    let mut out = String::new();
    for e in entries {
        match e.kind {
            Kind::Ok => out.push_str(&format!(
                "OK {} {}\n",
                e.http_status
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                e.url
            )),
            Kind::Broken => out.push_str(&format!(
                "BROKEN {} {}\n",
                e.http_status
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                e.url
            )),
            Kind::Redirect => {
                if e.redirect_chain.is_empty() {
                    out.push_str(&format!(
                        "REDIRECT {} {}\n",
                        e.http_status
                            .map(|c| c.to_string())
                            .unwrap_or_else(|| "-".to_string()),
                        e.url
                    ));
                } else {
                    let chain: Vec<String> = e
                        .redirect_chain
                        .iter()
                        .map(|h| format!("{} -> {}", h.status, h.url))
                        .collect();
                    out.push_str(&format!(
                        "REDIRECT {} {}\n",
                        e.http_status
                            .map(|c| c.to_string())
                            .unwrap_or_else(|| "-".to_string()),
                        chain.join(" ")
                    ));
                }
            }
            Kind::Error => out.push_str(&format!(
                "ERROR {} {}\n",
                e.error.as_deref().unwrap_or("unknown error"),
                e.url
            )),
            Kind::Malformed => out.push_str(&format!(
                "MALFORMED {} \"{}\" ({}:{})\n",
                e.error.as_deref().unwrap_or("bad url"),
                e.url,
                e.location.file,
                e.location.line,
            )),
        }
    }
    out
}

/// Summaries and JSON payload (SPEC §7).
#[derive(serde::Serialize)]
pub struct JsonReport<'a> {
    version: u32,
    files: Vec<&'a str>,
    summary: JsonSummary,
    checks: Vec<JsonCheck<'a>>,
}

#[derive(serde::Serialize)]
pub struct JsonSummary {
    total: usize,
    ok: usize,
    broken: usize,
    redirects: usize,
    errors: usize,
    malformed: usize,
}

#[derive(serde::Serialize)]
pub struct JsonCheck<'a> {
    url: &'a str,
    location: JsonLocation,
    status: &'a str,
    http_status: Option<u16>,
    redirect_chain: Vec<JsonHop>,
    error: Option<&'a str>,
}

#[derive(serde::Serialize)]
pub struct JsonLocation {
    file: String,
    line: usize,
    column: usize,
}

#[derive(serde::Serialize)]
pub struct JsonHop {
    status: u16,
    url: String,
}

/// Machine-readable JSON output (SPEC §7).
pub fn render_json(files: &[String], entries: &[Entry]) -> String {
    let s = summarize(entries);
    let checks: Vec<JsonCheck> = entries
        .iter()
        .map(|e| JsonCheck {
            url: &e.url,
            location: JsonLocation {
                file: e.location.file.clone(),
                line: e.location.line,
                column: e.location.column,
            },
            status: e.kind.as_str(),
            http_status: e.http_status,
            redirect_chain: e
                .redirect_chain
                .iter()
                .map(|h| JsonHop {
                    status: h.status,
                    url: h.url.clone(),
                })
                .collect(),
            error: match e.kind {
                // Malformed and non-Error kinds carry no transport error in
                // JSON (SPEC §7: `error` is for transport failures, else null).
                Kind::Error => e.error.as_deref(),
                _ => None,
            },
        })
        .collect();

    let report = JsonReport {
        version: 1,
        files: files.iter().map(String::as_str).collect(),
        summary: JsonSummary {
            total: s.total,
            ok: s.ok,
            broken: s.broken,
            redirects: s.redirects,
            errors: s.errors,
            malformed: s.malformed,
        },
        checks,
    };

    match serde_json::to_string_pretty(&report) {
        Ok(s) => s,
        Err(e) => format!("{{\"version\":1,\"error\":\"serialization failed: {e}\"}}"),
    }
}

/// Quiet mode: emit only the reportable (exit-code-visible) failures.
pub fn render_quiet(entries: &[Entry]) -> String {
    let mut out = String::new();
    for e in entries {
        match e.kind {
            Kind::Broken => out.push_str(&format!(
                "BROKEN {} {}\n",
                e.http_status
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                e.url
            )),
            Kind::Redirect if is_unfollowed(e) => out.push_str(&format!(
                "REDIRECT {} {}\n",
                e.http_status
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                e.url
            )),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::{RedirectHop, Status as CheckStatus};
    use std::collections::HashMap;

    fn loc(file: &str, line: usize, column: usize) -> Location {
        Location {
            file: file.to_string(),
            line,
            column,
        }
    }

    fn entry(url: &str, kind: Kind, status: Option<u16>, chain: Vec<RedirectHop>) -> Entry {
        Entry {
            url: url.to_string(),
            location: loc("a.md", 1, 1),
            kind,
            http_status: status,
            redirect_chain: chain,
            error: None,
        }
    }

    #[test]
    fn build_entries_sorts_by_location() {
        let occs = vec![
            Occurrence {
                url: "https://b.org".into(),
                location: loc("b.md", 2, 3),
            },
            Occurrence {
                url: "https://a.org".into(),
                location: loc("a.md", 1, 1),
            },
        ];
        let mut m = HashMap::new();
        m.insert(
            "https://b.org".to_string(),
            CheckResult {
                url: "https://b.org".into(),
                status: CheckStatus::Ok,
                http_status: Some(200),
                redirect_chain: vec![],
                redirect_unfollowed: false,
                error: None,
            },
        );
        m.insert(
            "https://a.org".to_string(),
            CheckResult {
                url: "https://a.org".into(),
                status: CheckStatus::Ok,
                http_status: Some(200),
                redirect_chain: vec![],
                redirect_unfollowed: false,
                error: None,
            },
        );
        let entries = build_entries(&occs, &[], &m);
        let files: Vec<&str> = entries.iter().map(|e| e.location.file.as_str()).collect();
        assert_eq!(files, vec!["a.md", "b.md"]);
    }

    #[test]
    fn summary_counts_and_exit_code() {
        let entries = vec![
            entry("https://ok.example", Kind::Ok, Some(200), vec![]),
            entry("https://bad.example", Kind::Broken, Some(404), vec![]),
            entry("https://err.example", Kind::Error, None, vec![]),
        ];
        let s = summarize(&entries);
        assert_eq!((s.ok, s.broken, s.errors, s.total), (1, 1, 1, 3));
        assert_eq!(exit_code(&entries), 1);
    }

    #[test]
    fn exit_code_zero_when_only_errors_and_redirects_followed() {
        let entries = vec![
            entry("https://ok.example", Kind::Ok, Some(200), vec![]),
            entry(
                "https://old.example",
                Kind::Redirect,
                Some(200),
                vec![RedirectHop {
                    status: 301,
                    url: "https://new.example".into(),
                }],
            ),
        ];
        assert_eq!(exit_code(&entries), 0);
    }

    #[test]
    fn exit_code_one_for_unfollowed_redirect() {
        let entries = vec![entry(
            "https://old.example",
            Kind::Redirect,
            Some(301),
            vec![],
        )];
        assert_eq!(exit_code(&entries), 1);
    }

    #[test]
    fn json_has_expected_shape() {
        let entries = vec![
            entry("https://ok.example", Kind::Ok, Some(200), vec![]),
            entry("https://bad.example", Kind::Broken, Some(404), vec![]),
        ];
        let files = vec!["a.md".to_string()];
        let json = render_json(&files, &entries);
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert_eq!(parsed["version"], 1);
        assert_eq!(parsed["summary"]["ok"], 1);
        assert_eq!(parsed["summary"]["broken"], 1);
        assert_eq!(parsed["checks"][1]["status"], "broken");
        assert_eq!(parsed["checks"][1]["http_status"], 404);
    }
}
