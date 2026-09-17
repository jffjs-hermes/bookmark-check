use std::{env, fs, process};

fn run(path: &str) -> Result<usize, String> {
    let contents =
        fs::read_to_string(path).map_err(|error| format!("failed to read {path}: {error}"))?;
    Ok(contents.lines().count())
}

fn main() {
    let Some(path) = env::args().nth(1) else {
        eprintln!("usage: bookmark-check <file.md>");
        process::exit(2);
    };

    match run(&path) {
        Ok(lines) => println!("parsed {lines} lines"),
        Err(error) => {
            eprintln!("{error}");
            process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::run;
    use std::fs;

    #[test]
    fn counts_lines() {
        let path = std::env::temp_dir().join("bookmark-check-test.md");
        fs::write(&path, "one\ntwo\nthree\n").expect("write fixture");
        assert_eq!(
            run(path.to_str().expect("utf-8 path")).expect("read fixture"),
            3
        );
        fs::remove_file(path).expect("remove fixture");
    }
}
