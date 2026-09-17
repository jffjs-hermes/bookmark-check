mod extract;

fn main() {
    let text = String::from("[a](https://example.com) <https://b.org> http://c.io");
    let res = extract::extract("docs.md", text.as_str());
    println!("occurrences: {}", res.occurrences.len());
    for occ in res.occurrences.iter() {
        println!(
            "  {} @ {}:{}",
            occ.url, occ.location.line, occ.location.column,
        );
    }
    println!("malformed: {}", res.malformed.len());
    for m in res.malformed.iter() {
        println!(
            "  {:?} {} @ {}:{}",
            m.problem, m.url, m.location.line, m.location.column,
        );
    }
    let uniq = extract::unique_urls(&res.occurrences);
    println!("unique: {}", uniq.len());
}
