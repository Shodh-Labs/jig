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
    // A quoted-empty first token (`"" …`) parses as a token but names no
    // program. Found by proptest in CI (minimal input: `""\u{b}`).
    if program.is_empty() {
        return Err("--stdio command names an empty program".to_string());
    }
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

/// The marker substituted for any value that could be a secret.
pub const REDACTED: &str = "<redacted>";

/// Substrings that mark a key (a CLI flag, an `env`-style assignment, or a URL
/// query parameter) whose *value* must never be printed. Matched
/// case-insensitively as a substring, so `--api-key`, `ACCESS_TOKEN` and
/// `?sig=` are all caught. Deliberately over-broad: printing `<redacted>` where
/// no secret existed is harmless, the converse is not.
const SECRET_KEY_MARKERS: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "pwd",
    "key",
    "auth",
    "credential",
    "signature",
    "sig",
];

/// Whether `key` names something whose value is a secret.
fn is_secret_key(key: &str) -> bool {
    let key = key.trim_start_matches('-').to_ascii_lowercase();
    SECRET_KEY_MARKERS.iter().any(|m| key.contains(m))
}

/// Redact the secrets out of a command line or URL so it is safe to print in a
/// report.
///
/// `jig check` states the invocation it measured, and a user-supplied
/// invocation can carry credentials: a URL with userinfo
/// (`https://user:pw@host/mcp`) or a query token (`?access_token=…`), or a flag
/// or `NAME=value` assignment whose key names a secret. Each whitespace-separated
/// token is rewritten independently:
///
/// * a token containing `://` has its URL userinfo dropped and every
///   secret-named query parameter's value replaced;
/// * any other `key=value` token with a secret-named key has its value replaced;
/// * a token *following* a secret-named flag is replaced, because `--api-key
///   sk-live-…` is the commonest way to pass one and carries no `=` to key on.
///   A token starting with `-` is treated as the next flag, not as a value, so
///   `--token --verbose` does not swallow the following flag.
///
/// Everything else is passed through untouched, so `npx -y @scope/pkg
/// --preset mail` survives verbatim. Whitespace runs collapse to a single space
/// — the reconstructed command line is for reading, not for re-execution.
///
/// Over-redaction is deliberate. A path that follows `--key-file` is replaced
/// too; printing one path too few is a cosmetic loss, printing one credential
/// too many is a leak into a report card people attach to issues.
pub fn redact_invocation(command: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut secret_flag_pending = false;
    for token in command.split_whitespace() {
        if secret_flag_pending && !token.starts_with('-') {
            out.push(REDACTED.to_string());
            secret_flag_pending = false;
            continue;
        }
        secret_flag_pending = is_bare_secret_flag(token);
        out.push(redact_token(token));
    }
    out.join(" ")
}

/// Whether `token` is a bare secret-named flag — one whose value is the *next*
/// token. An assignment (`--api-key=…`) is not bare: it carries its own value
/// and [`redact_assignment`] has already handled it.
fn is_bare_secret_flag(token: &str) -> bool {
    token.starts_with('-') && !token.contains('=') && is_secret_key(token)
}

/// Redact one whitespace-separated token of a command line.
fn redact_token(token: &str) -> String {
    if token.contains("://") {
        return redact_url_token(token);
    }
    redact_assignment(token)
}

/// Redact a `key=value` assignment whose key names a secret. Anything else
/// (including a bare flag, or an assignment with an empty value) is returned
/// unchanged.
fn redact_assignment(token: &str) -> String {
    match token.split_once('=') {
        Some((key, value)) if is_secret_key(key) && !value.is_empty() => {
            format!("{key}={REDACTED}")
        }
        _ => token.to_string(),
    }
}

/// Redact a token that contains a URL: userinfo is dropped wholesale, and every
/// secret-named query parameter's value is replaced. Scheme, host, port, path
/// and fragment are preserved — they are what identifies the endpoint measured.
fn redact_url_token(token: &str) -> String {
    let Some(sep) = token.find("://") else {
        return token.to_string();
    };
    let (scheme, rest) = token.split_at(sep + 3);
    let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(auth_end);
    // Userinfo is everything before the last `@`: username, password, or both.
    let authority = match authority.rfind('@') {
        Some(at) => format!("{REDACTED}@{}", &authority[at + 1..]),
        None => authority.to_string(),
    };
    let tail = match tail.find('?') {
        Some(q) => {
            let (path, query) = tail.split_at(q + 1);
            let (query, fragment) = match query.find('#') {
                Some(h) => query.split_at(h),
                None => (query, ""),
            };
            let params = query
                .split('&')
                .map(redact_assignment)
                .collect::<Vec<_>>()
                .join("&");
            format!("{path}{params}{fragment}")
        }
        None => tail.to_string(),
    };
    format!("{scheme}{authority}{tail}")
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

    // ---- redact_invocation --------------------------------------------------

    #[test]
    fn an_ordinary_stdio_invocation_is_printed_verbatim() {
        assert_eq!(
            redact_invocation("npx -y @softeria/ms-365-mcp-server --preset mail"),
            "npx -y @softeria/ms-365-mcp-server --preset mail"
        );
    }

    #[test]
    fn a_url_keeps_its_endpoint_and_its_harmless_query() {
        assert_eq!(
            redact_invocation("https://api.example.com:8443/mcp?mode=lite#frag"),
            "https://api.example.com:8443/mcp?mode=lite#frag"
        );
    }

    #[test]
    fn url_userinfo_is_redacted() {
        assert_eq!(
            redact_invocation("https://alice:hunter2@example.com/mcp"),
            "https://<redacted>@example.com/mcp"
        );
        // A bare username, with no password, is userinfo too.
        assert_eq!(
            redact_invocation("https://alice@example.com/mcp"),
            "https://<redacted>@example.com/mcp"
        );
    }

    #[test]
    fn a_url_query_token_is_redacted_and_the_rest_of_the_query_survives() {
        assert_eq!(
            redact_invocation("https://example.com/mcp?mode=lite&access_token=sk-abc123&v=2"),
            "https://example.com/mcp?mode=lite&access_token=<redacted>&v=2"
        );
        // Fragments are preserved and never swallow the redaction.
        assert_eq!(
            redact_invocation("https://example.com/mcp?api_key=sk-1#top"),
            "https://example.com/mcp?api_key=<redacted>#top"
        );
    }

    #[test]
    fn a_secret_bearing_flag_or_assignment_is_redacted() {
        assert_eq!(
            redact_invocation("npx -y srv --api-key=sk-abc GITHUB_TOKEN=ghp_xyz --preset mail"),
            "npx -y srv --api-key=<redacted> GITHUB_TOKEN=<redacted> --preset mail"
        );
    }

    #[test]
    fn a_secret_passed_as_the_next_argument_is_redacted() {
        // The commonest shape, and the one an `=`-only rule misses entirely.
        assert_eq!(
            redact_invocation("npx -y srv --api-key sk-live-abc123 --preset mail"),
            "npx -y srv --api-key <redacted> --preset mail"
        );
        assert_eq!(
            redact_invocation("srv --token ghp_xyz"),
            "srv --token <redacted>"
        );
    }

    #[test]
    fn a_secret_flag_does_not_swallow_the_flag_that_follows_it() {
        // `--token` with its value omitted must not redact `--verbose`, and must
        // not hide that the next flag was passed at all.
        assert_eq!(
            redact_invocation("srv --token --verbose"),
            "srv --token --verbose"
        );
    }

    #[test]
    fn an_ordinary_flags_value_survives_redaction() {
        // Over-redaction is acceptable for secrets; it is not acceptable for
        // the arguments that identify what was measured.
        assert_eq!(
            redact_invocation("npx -y srv --preset mail,calendar --port 8080"),
            "npx -y srv --preset mail,calendar --port 8080"
        );
    }

    #[test]
    fn redaction_never_panics_on_degenerate_input() {
        for s in [
            "",
            "://",
            "http://",
            "a=",
            "=b",
            "?=&=",
            "https://@",
            "x://?",
        ] {
            let _ = redact_invocation(s);
        }
    }
}
