use url::Url;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    pub file: String,
    pub line: usize,
    pub column: usize,
}

#[derive(Clone)]
pub struct Occurrence {
    pub url: String,
    pub location: Location,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    EmptyUrl,
    InvalidUrl,
    MissingHost,
    UnmatchedBrackets,
    UnbalancedTitle,
}

#[derive(Clone)]
pub struct Malformed {
    pub url: String,
    pub problem: Problem,
    pub location: Location,
}

#[derive(Clone)]
pub struct Extraction {
    pub occurrences: Vec<Occurrence>,
    pub malformed: Vec<Malformed>,
}

// substring helper: returns owned String for byte range [from..to).
fn sub(text: &str, from: usize, to: usize) -> String {
    let (_, rest): (&str, &str) = text.split_at(from);
    let (grab, _): (&str, &str) = rest.split_at(to - from);
    String::from(grab)
}

pub fn extract(file: &str, text: &str) -> Extraction {
    let mut result = Extraction {
        occurrences: Vec::new(),
        malformed: Vec::new(),
    };
    let mut in_fence = false;
    for (_idx, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if (trimmed.starts_with("```")) || (trimmed.starts_with("~~~")) {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        process_line(file, _idx + 1, line, &mut result);
    }
    result
}

fn classify(file: &str, line: usize, column: usize, url: String, out: &mut Extraction) {
    if url.is_empty() {
        push_malformed(file, line, column, url, Problem::EmptyUrl, out);
        return;
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        classify_http(file, line, column, url, out);
        return;
    }
    // Non-HTTP(S) forms: silently skip recognised schemes, `#fragment` anchors
    // and relative paths; classify unknown scheme-like tokens that fail parsing
    // (e.g. `htp:/x`) as invalid.
    match Url::parse(url.as_str()) {
        Ok(u) => {
            let scheme = u.scheme();
            // non-HTTP schemes and relative paths / fragments are out of scope
            // and silently skipped
            if !(scheme == "mailto" || scheme == "ftp" || scheme == "file" || scheme.is_empty()) {
                // any other parsed scheme (e.g. `tel:`, `data:`) is also
                // non-HTTP and therefore out of scope
            }
        }
        Err(_) => {
            // If it looks scheme-like (has a `:` before any `/`/`?`/`#`), the
            // failing parse is a malformed URL rather than a relative path.
            if looks_scheme_like(url.as_str()) {
                push_malformed(file, line, column, url, Problem::InvalidUrl, out);
            }
            // otherwise a relative path / anchor: silently skipped
        }
    }
}

fn classify_http(file: &str, line: usize, column: usize, url: String, out: &mut Extraction) {
    match Url::parse(url.as_str()) {
        Ok(u) => {
            if u.host_str().is_none() {
                push_malformed(file, line, column, url, Problem::MissingHost, out);
            } else {
                push_occurrence(file, line, column, url, out);
            }
        }
        Err(_) => {
            // a scheme-prefixed http URL that failed to parse: if nothing (or
            // only / ? #) follows the scheme it has no host, else it is invalid.
            let after = strip_scheme_prefix(url.as_str());
            let head = after.chars().next();
            if after.is_empty() || head == Some('/') || head == Some('?') || head == Some('#') {
                push_malformed(file, line, column, url, Problem::MissingHost, out);
            } else {
                push_malformed(file, line, column, url, Problem::InvalidUrl, out);
            }
        }
    }
}

fn strip_scheme_prefix(url: &str) -> &str {
    if let Some(rest) = url.strip_prefix("https://") {
        rest
    } else if let Some(rest) = url.strip_prefix("http://") {
        rest
    } else {
        url
    }
}

// true when the token contains a ':' before its first '/', '?', or '#'.
fn looks_scheme_like(url: &str) -> bool {
    for c in url.chars() {
        match c {
            ':' => return true,
            '/' | '?' | '#' => return false,
            _ => {}
        }
    }
    false
}

fn push_occurrence(file: &str, line: usize, column: usize, url: String, out: &mut Extraction) {
    let occ = Occurrence {
        url,
        location: Location {
            file: String::from(file),
            line,
            column,
        },
    };
    out.occurrences.push(occ);
}

fn push_malformed(
    file: &str,
    line: usize,
    column: usize,
    url: String,
    problem: Problem,
    out: &mut Extraction,
) {
    let m = Malformed {
        url,
        problem,
        location: Location {
            file: String::from(file),
            line,
            column,
        },
    };
    out.malformed.push(m);
}

fn process_line(file: &str, line: usize, text: &str, out: &mut Extraction) {
    let n: usize = text.len();
    let mut idx: usize = 0;
    let mut in_code = false;
    while idx < n {
        let ch = text.chars().nth(idx).unwrap();
        if ch == '`' {
            in_code = !in_code;
            idx += 1;
            continue;
        }
        if !in_code {
            // reference-style definition [label]: URL at line start
            if idx == 0 {
                if let Some((end, url)) = read_refdef(text) {
                    classify(file, line, 1, url, out);
                    idx = end;
                    continue;
                }
            }
            // autolink <http(s)://...>
            if text[idx..].starts_with("<") {
                if let Some((end, inner)) = read_angle(text, idx) {
                    classify(file, line, idx + 1, inner, out);
                    idx = end;
                    continue;
                }
            }
            // inline link [text](url)
            if text[idx..].starts_with("[") {
                if let Some((end, url, problem)) = read_inline(text, idx) {
                    if let Some(p) = problem {
                        push_malformed(file, line, idx + 1, url, p, out);
                    } else {
                        classify(file, line, idx + 1, url, out);
                    }
                    idx = end;
                    continue;
                }
            }
            // bare http(s):// URLs
            if text[idx..].starts_with("http://") || text[idx..].starts_with("https://") {
                let (end, url): (usize, String) = read_bare(text, idx);
                classify(file, line, idx + 1, url, out);
                idx = end;
                continue;
            }
        }
        idx += 1;
    }
}

fn read_angle(text: &str, open: usize) -> Option<(usize, String)> {
    let close_rel = text[(open + 1)..].find(">")?;
    let close = open + 1 + close_rel;
    let inner = sub(text, open + 1, close);
    if !(inner.starts_with("http://") || inner.starts_with("https://")) {
        return None;
    }
    Some((close + 1, inner))
}

// reference definition `[label]: URL` (optionally `<URL>`) at line start.
// returns (index past the URL, url-string)
fn read_refdef(text: &str) -> Option<(usize, String)> {
    let close = text.find(']')?;
    // must be followed by ':' then whitespace then the URL
    if !text[close + 1..].starts_with(':') {
        return None;
    }
    let after_colon = close + 2;
    let rest = text[after_colon..].trim_start();
    let lead_offset = text[after_colon..].len() - rest.len();
    let url: String;
    let url_len: usize;
    if rest.starts_with('<') {
        let inner_end = rest.find('>')?;
        url = String::from(&rest[1..inner_end]);
        url_len = inner_end + 1;
    } else {
        let url_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        url = String::from(&rest[..url_end]);
        url_len = url_end;
    }
    let end = after_colon + lead_offset + url_len;
    Some((end, url))
}

// returns (index after closing paren, url-or-raw, optional problem)
fn read_inline(text: &str, open: usize) -> Option<(usize, String, Option<Problem>)> {
    let n: usize = text.len();
    // find ']' then '('
    let mut bracket: Option<usize> = None;
    let mut scan: usize = open + 1;
    while scan < n {
        if text.chars().nth(scan).unwrap() == ']' {
            bracket = Some(scan);
            break;
        }
        scan += 1;
    }
    bracket?;
    let b = bracket.unwrap();
    if !text[(b + 1)..].starts_with('(') {
        return None;
    }
    let paren_open = b + 1;
    let mut depth: usize = 1;
    let mut j: usize = paren_open + 1;
    let mut in_quotes = false;
    while j < n {
        let c = text.chars().nth(j).unwrap();
        if in_quotes {
            if c == '"' {
                in_quotes = false;
            }
            j += 1;
            continue;
        }
        if c == '"' {
            in_quotes = true;
            j += 1;
            continue;
        }
        if c == '(' {
            depth += 1;
        } else if c == ')' {
            depth -= 1;
            if depth == 0usize {
                break;
            }
        }
        j += 1;
    }
    if depth != 0usize {
        let problem = if in_quotes {
            Problem::UnbalancedTitle
        } else {
            Problem::UnmatchedBrackets
        };
        return Some((n, String::from(&text[(paren_open + 1)..]), Some(problem)));
    }
    let inner = sub(text, paren_open + 1, j);
    let tokens = split_tokens(inner.as_str());
    if tokens.is_empty() {
        return Some((j + 1, String::from(""), Some(Problem::EmptyUrl)));
    }
    // first whitespace-separated token is the url; a trailing "..." quoted token
    // is the title, which is out of scope for the URL itself.
    let url_token: String = tokens[0].clone();
    if tokens.len() >= 2usize {
        let last: &String = &tokens[tokens.len() - 1];
        if last.starts_with("\"") && !last.ends_with("\"") && tokens.len() == 2usize {
            return Some((j + 1, url_token, Some(Problem::UnbalancedTitle)));
        }
    }
    Some((j + 1, url_token, None))
}

fn read_bare(text: &str, open: usize) -> (usize, String) {
    let n = text.len();
    let mut end: usize = open;
    while end < n {
        let c = text.chars().nth(end).unwrap();
        if c == ' ' || c == '\t' || c == '<' || c == '>' || c == ')' {
            break;
        }
        end += 1;
    }
    let mut raw = sub(text, open, end);
    // strip trailing punctuation . , ; : ! ?
    while !raw.is_empty() {
        let lastrel = raw.len() - 1usize;
        let lc = raw.chars().nth(lastrel).unwrap();
        if lc == '.' || lc == ',' || lc == ';' || lc == ':' || lc == '!' || lc == '?' {
            raw = sub(raw.as_str(), 0, lastrel);
        } else {
            break;
        }
    }
    (open + raw.len(), raw)
}

// split on whitespace into a Vec<String>
fn split_tokens(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut word = String::new();
    let n = s.len();
    let mut i: usize = 0;
    while i <= n {
        let is_sep = if i == n {
            true
        } else {
            let c = s.chars().nth(i).unwrap();
            c == ' ' || c == '\t' || c == '\n'
        };
        if is_sep {
            if !word.is_empty() {
                out.push(word.clone());
                word = String::new();
            }
        } else {
            word.push(s.chars().nth(i).unwrap());
        }
        i += 1;
    }
    out
}

// dedup URLs for checking; occurrences remain in the Extraction.
pub fn unique_urls(occurrences: &[Occurrence]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    let mut result: Vec<String> = Vec::new();
    for occ in occurrences.iter() {
        let u: String = occ.url.clone();
        let mut dup = false;
        for w in seen.iter() {
            if *w == u {
                dup = true;
                break;
            }
        }
        if !dup {
            seen.push(u.clone());
            result.push(u);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(res: &Extraction) -> Vec<String> {
        res.occurrences.iter().map(|o| o.url.clone()).collect()
    }

    fn problems(res: &Extraction) -> Vec<Problem> {
        res.malformed.iter().map(|m| m.problem.clone()).collect()
    }

    #[test]
    fn inline_link() {
        let r = extract("doc.md", "a [label](https://example.com) b");
        assert_eq!(urls(&r), vec!["https://example.com"]);
        assert_eq!(r.malformed.len(), 0);
        let o = &r.occurrences[0];
        assert_eq!(o.location.file, "doc.md");
        assert_eq!(o.location.line, 1);
        assert_eq!(o.location.column, 3);
    }

    #[test]
    fn inline_link_with_title() {
        let r = extract("doc.md", "[a](https://example.com \"the title\")");
        assert_eq!(urls(&r), vec!["https://example.com"]);
    }

    #[test]
    fn autolink() {
        let r = extract("doc.md", "see <https://example.com> now");
        assert_eq!(urls(&r), vec!["https://example.com"]);
        assert_eq!(r.occurrences[0].location.column, 5);
    }

    #[test]
    fn reference_definition() {
        let r = extract("doc.md", "[ref]: https://example.com\nuse [ref] later");
        assert_eq!(urls(&r), vec!["https://example.com"]);
        assert_eq!(r.occurrences[0].location.line, 1);
        assert_eq!(r.occurrences[0].location.column, 1);
    }

    #[test]
    fn reference_definition_angled() {
        let r = extract("doc.md", "[ref]: <https://example.com/path> trailing");
        assert_eq!(urls(&r), vec!["https://example.com/path"]);
    }

    #[test]
    fn bare_url() {
        let r = extract("doc.md", "visit http://example.com today");
        assert_eq!(urls(&r), vec!["http://example.com"]);
        assert_eq!(r.occurrences[0].location.column, 7);
    }

    #[test]
    fn bare_url_strips_trailing_punct() {
        let r = extract("doc.md", "see https://example.com, and https://x.org/end.");
        assert_eq!(urls(&r), vec!["https://example.com", "https://x.org/end"]);
    }

    #[test]
    fn multiple_on_one_line_with_locations() {
        let r = extract("doc.md", " [a](https://a.com)\n  https://b.com");
        let o = &r.occurrences;
        assert_eq!(o.len(), 2);
        assert_eq!(o[0].location.line, 1);
        assert_eq!(o[0].location.column, 2);
        assert_eq!(o[1].location.line, 2);
        assert_eq!(o[1].location.column, 3);
    }

    #[test]
    fn fenced_code_excluded() {
        let r = extract(
            "doc.md",
            "```\n[a](https://excluded.com)\n```\nvalid [b](https://ok.com)",
        );
        assert_eq!(urls(&r), vec!["https://ok.com"]);
    }

    #[test]
    fn inline_code_excluded() {
        let r = extract(
            "doc.md",
            "`[a](https://excluded.com)` real [b](https://ok.com)",
        );
        assert_eq!(urls(&r), vec!["https://ok.com"]);
    }

    #[test]
    fn non_http_schemes_skipped() {
        let r = extract(
            "doc.md",
            "[mail](mailto:a@b.c) [ftp](ftp://x) [f](file:///x) [rel](sub/page)",
        );
        assert_eq!(r.occurrences.len(), 0);
        assert_eq!(r.malformed.len(), 0);
    }

    #[test]
    fn http_missing_host_is_malformed() {
        let r = extract("doc.md", "[a](http://)");
        assert_eq!(r.occurrences.len(), 0);
        assert_eq!(problems(&r), vec![Problem::MissingHost]);
    }

    #[test]
    fn invalid_url_is_malformed() {
        let r = extract("doc.md", "[a](http://%zz) [b](http://[bad])");
        assert_eq!(r.occurrences.len(), 0);
        assert_eq!(problems(&r), vec![Problem::InvalidUrl, Problem::InvalidUrl]);
    }

    #[test]
    fn empty_url_is_malformed() {
        let r = extract("doc.md", "[a]()");
        assert_eq!(problems(&r), vec![Problem::EmptyUrl]);
        assert_eq!(r.malformed[0].location.column, 1);
    }

    #[test]
    fn unmatched_brackets_is_malformed() {
        let r = extract("doc.md", "[a](https://example.com");
        assert_eq!(problems(&r), vec![Problem::UnmatchedBrackets]);
    }

    #[test]
    fn unbalanced_title_is_malformed() {
        let r = extract("doc.md", "[a](https://example.com \"title)");
        assert_eq!(problems(&r), vec![Problem::UnbalancedTitle]);
    }

    #[test]
    fn malformed_never_occurrence() {
        let r = extract("doc.md", "[a](http://)\n[b](http://%zz)");
        assert_eq!(r.occurrences.len(), 0);
        assert_eq!(r.malformed.len(), 2);
    }

    #[test]
    fn dedup_preserves_occurrences() {
        let r = extract("doc.md", "[a](https://x.com)\n[b](https://x.com)");
        assert_eq!(r.occurrences.len(), 2);
        assert_eq!(unique_urls(&r.occurrences), vec!["https://x.com"]);
    }

    #[test]
    fn multidoc_locations() {
        let a = extract("a.md", "[x](https://a.com)");
        let b = extract("b.md", "[y](https://b.com)");
        assert_eq!(a.occurrences[0].location.file, "a.md");
        assert_eq!(b.occurrences[0].location.file, "b.md");
    }
}
