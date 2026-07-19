//! Pure, dependency-light parsing helpers shared by the `jig` binary and by
//! the property-based and fuzz test harnesses.
//!
//! These live in a library target (alongside the `jig` binary in the same
//! crate) specifically so the milestone's property tests and `cargo-fuzz`
//! targets can exercise them directly — a bin-only module is unreachable from
//! another crate. They take arbitrary, possibly hostile input (a user-typed
//! `--stdio` command string, repeated `--header` flags) and must never panic:
//! every malformed input is a typed `Err`, never an unwind.

/// Split a single `--stdio` command string into program + args.
///
/// Supports double-quoted segments so paths containing spaces survive
/// (e.g. `"C:\\Program Files\\srv.exe" --flag`). This is a small, purpose-built
/// splitter rather than a full shell parser.
///
/// # Errors
///
/// Returns `Err` on unbalanced quotes or an empty command — never panics,
/// whatever bytes the string contains.
pub fn split_command(input: &str) -> Result<(String, Vec<String>), String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut has_token = false;

    for ch in input.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                has_token = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if has_token {
                    tokens.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            c => {
                current.push(c);
                has_token = true;
            }
        }
    }
    if in_quotes {
        return Err("unbalanced quotes in --stdio command".to_string());
    }
    if has_token {
        tokens.push(current);
    }

    let mut it = tokens.into_iter();
    let program = it
        .next()
        .ok_or_else(|| "--stdio command was empty".to_string())?;
    Ok((program, it.collect()))
}

/// Parse repeated `--header "Name: Value"` strings into (name, value) pairs.
///
/// The value may itself contain colons (e.g. a URL), so only the first colon
/// splits. Surrounding whitespace on the value is trimmed.
///
/// # Errors
///
/// Returns `Err` on a header with no colon or an empty name — never panics.
pub fn parse_headers(raw: &[String]) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::with_capacity(raw.len());
    for h in raw {
        let (name, value) = h
            .split_once(':')
            .ok_or_else(|| format!("invalid --header '{h}': expected \"Name: Value\""))?;
        let name = name.trim();
        let value = value.trim();
        if name.is_empty() {
            return Err(format!("invalid --header '{h}': empty header name"));
        }
        out.push((name.to_string(), value.to_string()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_plain_command() {
        let (p, a) = split_command("server --flag value").unwrap();
        assert_eq!(p, "server");
        assert_eq!(a, vec!["--flag", "value"]);
    }

    #[test]
    fn split_quoted_path_with_spaces() {
        let (p, a) = split_command("\"C:\\Program Files\\srv.exe\" --x 1").unwrap();
        assert_eq!(p, "C:\\Program Files\\srv.exe");
        assert_eq!(a, vec!["--x", "1"]);
    }

    #[test]
    fn split_empty_is_error() {
        assert!(split_command("   ").is_err());
    }

    #[test]
    fn split_unbalanced_quote_is_error() {
        assert!(split_command("\"oops").is_err());
    }

    #[test]
    fn headers_parse_name_value_pairs() {
        let hs = parse_headers(&["Authorization: Bearer x".to_string()]).unwrap();
        assert_eq!(
            hs,
            vec![("Authorization".to_string(), "Bearer x".to_string())]
        );
    }

    #[test]
    fn header_value_may_contain_colons() {
        let hs = parse_headers(&["X-Url: https://a.b/c".to_string()]).unwrap();
        assert_eq!(hs[0].1, "https://a.b/c");
    }

    #[test]
    fn header_without_colon_is_error() {
        assert!(parse_headers(&["nope".to_string()]).is_err());
    }

    #[test]
    fn header_with_empty_name_is_error() {
        assert!(parse_headers(&[": value".to_string()]).is_err());
    }
}
