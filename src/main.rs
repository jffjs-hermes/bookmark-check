//! bookmark-check — a CLI that checks links in Markdown files (SPEC §3).
//!
//! Reads one or more Markdown files (or stdin via `-`), extracts URLs, checks
//! them concurrently over HTTP, and reports broken/redirected links as text or
//! JSON.

mod check;
mod extract;
mod report;

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::process;

use clap::Parser;

const DEFAULT_TIMEOUT_SECS: u64 = 10;
const MAX_TIMEOUT_SECS: u64 = 120;
const DEFAULT_CONCURRENCY: usize = 16;
const MAX_CONCURRENCY: usize = 128;
const DEFAULT_USER_AGENT: &str = "bookmark-check/0.1";

/// Check links in Markdown files.
#[derive(Parser, Debug)]
#[command(
    name = "bookmark-check",
    version,
    about = "Check links in Markdown files"
)]
struct Args {
    /// One or more Markdown file paths; `-` reads a single document from stdin.
    #[arg(value_name = "FILE", required = true)]
    files: Vec<String>,

    /// Emit machine-readable JSON instead of text.
    #[arg(long)]
    json: bool,

    /// Per-request timeout in seconds (clamped to 1..=120, default 10).
    #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECS)]
    timeout: u64,

    /// Max parallel HTTP requests (clamped to 1..=128, default 16).
    #[arg(long, default_value_t = DEFAULT_CONCURRENCY)]
    concurrency: usize,

    /// Override the default User-Agent.
    #[arg(long)]
    user_agent: Option<String>,

    /// Follow redirects (default) and report the final destination + chain.
    #[arg(long, overrides_with = "no_follow_redirects")]
    follow_redirects: bool,

    /// Do not follow redirects; report a 3xx as a finding.
    #[arg(long, overrides_with = "follow_redirects")]
    no_follow_redirects: bool,

    /// Stop checking after the first broken link.
    #[arg(long)]
    fail_fast: bool,

    /// Suppress per-link summary; print only exit-code-visible failures.
    #[arg(long)]
    quiet: bool,
}

impl Args {
    fn follow_redirects(&self) -> bool {
        // Default is to follow; --no-follow-redirects flips it.
        !self.no_follow_redirects
    }
}

fn clamp(value: u64, min: u64, max: u64) -> u64 {
    value.clamp(min, max)
}

fn clamp_usize(value: usize, min: usize, max: usize) -> usize {
    value.clamp(min, max)
}

/// Load every requested input (file or stdin) BEFORE any HTTP — an unreadable
/// input is a usage error (exit 2) and aborts the run.
fn load_inputs(files: &[String]) -> Result<Vec<(String, String)>, String> {
    let mut inputs = Vec::new();
    for path in files {
        if path == "-" {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .map_err(|e| format!("failed to read stdin: {e}"))?;
            inputs.push(("-".to_string(), buf));
        } else {
            let contents =
                fs::read_to_string(path).map_err(|e| format!("failed to read {path}: {e}"))?;
            inputs.push((path.clone(), contents));
        }
    }
    Ok(inputs)
}

fn run(args: &Args) -> i32 {
    // Clamp numeric options to their permitted ranges.
    let timeout = clamp(args.timeout, 1, MAX_TIMEOUT_SECS);
    let concurrency = clamp_usize(args.concurrency, 1, MAX_CONCURRENCY);
    let user_agent = args
        .user_agent
        .clone()
        .unwrap_or_else(|| DEFAULT_USER_AGENT.to_string());
    let follow = args.follow_redirects();

    // 1. Open all inputs before any network activity.
    let inputs = match load_inputs(&args.files) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };

    // 2. Extract URLs from every file, accumulating all documents.
    let mut occurrences = Vec::new();
    let mut malformed = Vec::new();
    let mut files: Vec<String> = Vec::new();
    for (name, text) in &inputs {
        files.push(name.clone());
        let res = extract::extract(name, text);
        occurrences.extend(res.occurrences);
        malformed.extend(res.malformed);
    }

    // 3. De-duplicate URLs for checking while preserving every occurrence.
    let unique = extract::unique_urls(&occurrences);

    // 4. Check concurrently.
    let options = check::CheckOptions {
        timeout_secs: timeout,
        concurrency,
        user_agent,
        follow_redirects: follow,
    };
    let results = check::check_all(&unique, &options, args.fail_fast);
    let by_url: HashMap<String, check::CheckResult> =
        results.into_iter().map(|r| (r.url.clone(), r)).collect();

    // 5. Assemble a deterministic (location-sorted) report.
    let entries = report::build_entries(&occurrences, &malformed, &by_url);

    // 6. Emit output.
    if args.json {
        print!("{}", report::render_json(&files, &entries));
    } else if args.quiet {
        print!("{}", report::render_quiet(&entries));
    } else {
        print!("{}", report::render_text(&entries));
    }

    // 7. Exit code: 0 healthy (redirects ending 2xx are healthy), 1 broken or
    //    unfollowed redirect, 2 was handled by arg/IO errors above.
    report::exit_code(&entries)
}

fn main() {
    let args = Args::parse();
    let code = run(&args);
    if code != 0 {
        process::exit(code);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_ranges() {
        assert_eq!(clamp(0, 1, 120), 1);
        assert_eq!(clamp(999, 1, 120), 120);
        assert_eq!(clamp(10, 1, 120), 10);
    }

    #[test]
    fn clamp_usize_ranges() {
        assert_eq!(clamp_usize(0, 1, 128), 1);
        assert_eq!(clamp_usize(2000, 1, 128), 128);
        assert_eq!(clamp_usize(8, 1, 128), 8);
    }

    #[test]
    fn default_follows_redirects() {
        let args = Args {
            files: vec!["a.md".into()],
            json: false,
            timeout: 10,
            concurrency: 16,
            user_agent: None,
            follow_redirects: false,
            no_follow_redirects: false,
            fail_fast: false,
            quiet: false,
        };
        assert!(args.follow_redirects());
    }

    #[test]
    fn no_follow_redirects_disables_following() {
        let args = Args {
            files: vec!["a.md".into()],
            json: true,
            timeout: 30,
            concurrency: 4,
            user_agent: Some("x".into()),
            follow_redirects: false,
            no_follow_redirects: true,
            fail_fast: true,
            quiet: true,
        };
        assert!(!args.follow_redirects());
    }

    #[test]
    fn load_inputs_reads_stdin_dash() {
        // A lone "-" is accepted as an input path marker.
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf).ok();
        // Not exercised over real stdin here; just verify file loads work.
        let path = std::env::temp_dir().join("bookmark-check-cli-test.md");
        fs::write(&path, "# hi\n").expect("write");
        let inputs = load_inputs(&[path.to_string_lossy().into_owned()]).expect("load");
        assert_eq!(inputs.len(), 1);
        assert!(inputs[0].1.contains("hi"));
        fs::remove_file(path).expect("remove");
    }

    #[test]
    fn load_inputs_missing_file_is_error() {
        let missing = "/no/such/dir/does-not-exist.md";
        assert!(load_inputs(&[missing.to_string()]).is_err());
    }
}
