//! Sender verification from `Authentication-Results` (RFC 8601; PLAN section 3.3).
//!
//! Only the topmost `Authentication-Results` header counts, and only if its authserv-id is the
//! configured one: the receiving server adds its header above everything the sender wrote, so a
//! header further down (or one naming another server) may be forged. That holds only if the
//! configured server adds its header to every message delivered to the mailbox and removes
//! incoming ones carrying its authserv-id (RFC 8601 section 5). The sender is authenticated if
//! that header has `dmarc=pass` with `header.from` naming the `From:` domain (so the server and
//! this plugin agree on which domain passed), or a `dkim=pass` whose signing domain is aligned
//! with the `From:` domain.
//!
//! Alignment without a public suffix list: the `From:` domain must equal the signing domain or be
//! a subdomain of it. Whoever controls a zone controls its subdomains anyway, so that never trusts
//! a signer that couldn't publish keys for the `From:` domain itself. It's stricter than DMARC's
//! relaxed alignment, which also accepts a signer that is a subdomain or sibling of the `From:`
//! domain under the same organizational domain; such mail still passes when the receiving server
//! reports `dmarc=pass`. A signing domain must have at least two labels.

/// One `method=result` and its properties (`header.d=...`, `reason=...`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MethodResult {
    /// Lowercase, without a `/version`.
    pub method: String,
    /// Lowercase.
    pub result: String,
    /// Lowercase keys (`header.d`), values as written (unquoted).
    pub properties: Vec<(String, String)>,
}

impl MethodResult {
    fn property(&self, key: &str) -> Option<&str> {
        self.properties
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// A parsed `Authentication-Results` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuthResults {
    pub authserv_id: String,
    pub results: Vec<MethodResult>,
}

/// How a sender was verified, or why not; for logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// `dmarc=pass`.
    Dmarc,
    /// An aligned `dkim=pass` for this signing domain.
    Dkim(String),
    /// Not authenticated, and why.
    Unauthenticated(String),
}

impl Verdict {
    pub(crate) fn is_authenticated(&self) -> bool {
        !matches!(self, Self::Unauthenticated(_))
    }
}

/// Verifies a sender from the topmost `Authentication-Results` value (unfolded), against the
/// configured authserv-id and the `From:` domain.
pub(crate) fn verify(topmost: Option<&str>, authserv_id: &str, from_domain: &str) -> Verdict {
    let Some(value) = topmost else {
        return Verdict::Unauthenticated("no Authentication-Results header".into());
    };
    let Some(results) = parse(value) else {
        return Verdict::Unauthenticated("the topmost Authentication-Results doesn't parse".into());
    };
    if !results.authserv_id.eq_ignore_ascii_case(authserv_id) {
        return Verdict::Unauthenticated(format!(
            "the topmost Authentication-Results is from {:?}, not {authserv_id:?}",
            results.authserv_id
        ));
    }
    let from_domain = normalize_domain(from_domain);
    let dmarc = results.results.iter().any(|r| {
        r.method == "dmarc"
            && r.result == "pass"
            && r.property("header.from")
                .is_some_and(|domain| normalize_domain(domain) == from_domain)
    });
    if dmarc {
        return Verdict::Dmarc;
    }
    let dkim = results
        .results
        .iter()
        .filter(|r| r.method == "dkim" && r.result == "pass")
        .filter_map(signing_domain)
        .find(|domain| aligned(&from_domain, domain));
    match dkim {
        Some(domain) => Verdict::Dkim(domain),
        None => Verdict::Unauthenticated("no dmarc=pass or aligned dkim=pass".into()),
    }
}

/// A `dkim` result's signing domain: `header.d`, else the domain of `header.i` (the signer's
/// identity, `d` or a subdomain of it).
fn signing_domain(result: &MethodResult) -> Option<String> {
    let domain = match result.property("header.d") {
        Some(d) => d,
        None => result.property("header.i")?.rsplit_once('@')?.1,
    };
    let domain = normalize_domain(domain);
    (!domain.is_empty()).then_some(domain)
}

fn normalize_domain(domain: &str) -> String {
    domain.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Whether `from_domain` equals `signer` or is a subdomain of it (both normalized).
fn aligned(from_domain: &str, signer: &str) -> bool {
    if !signer.contains('.') || signer.starts_with('.') {
        return false;
    }
    from_domain == signer
        || from_domain
            .strip_suffix(signer)
            .is_some_and(|prefix| prefix.ends_with('.') && prefix.len() > 1)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    /// A run of characters other than whitespace, `;`, `=`, `(`, and `"`.
    Word(String),
    /// A quoted string, unescaped.
    Quoted(String),
    Equals,
    Semicolon,
}

/// Splits a header value into tokens, dropping comments (nested, with `\` escapes). `None` if a
/// comment or quoted string isn't closed.
fn tokenize(value: &str) -> Option<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut chars = value.chars().peekable();
    let mut word = String::new();
    let flush = |word: &mut String, tokens: &mut Vec<Token>| {
        if !word.is_empty() {
            tokens.push(Token::Word(std::mem::take(word)));
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '(' => {
                flush(&mut word, &mut tokens);
                let mut depth = 1;
                while depth > 0 {
                    match chars.next()? {
                        '\\' => {
                            chars.next()?;
                        }
                        '(' => depth += 1,
                        ')' => depth -= 1,
                        _ => {}
                    }
                }
            }
            '"' => {
                flush(&mut word, &mut tokens);
                let mut quoted = String::new();
                loop {
                    match chars.next()? {
                        '\\' => quoted.push(chars.next()?),
                        '"' => break,
                        c => quoted.push(c),
                    }
                }
                tokens.push(Token::Quoted(quoted));
            }
            ';' => {
                flush(&mut word, &mut tokens);
                tokens.push(Token::Semicolon);
            }
            '=' => {
                flush(&mut word, &mut tokens);
                tokens.push(Token::Equals);
            }
            c if c.is_whitespace() => flush(&mut word, &mut tokens),
            c => word.push(c),
        }
    }
    flush(&mut word, &mut tokens);
    Some(tokens)
}

/// Parses an `Authentication-Results` value. `None` if the authserv-id part is malformed; a
/// malformed result is skipped (it can only make the sender less authenticated).
pub(crate) fn parse(value: &str) -> Option<AuthResults> {
    let tokens = tokenize(value)?;
    let mut segments = tokens.split(|t| *t == Token::Semicolon);
    // authserv-id [ CFWS authres-version ]
    let head = segments.next()?;
    let authserv_id = match head {
        [Token::Word(id) | Token::Quoted(id)]
        | [Token::Word(id) | Token::Quoted(id), Token::Word(_)] => id.clone(),
        _ => return None,
    };
    let results = segments.filter_map(parse_result).collect();
    Some(AuthResults {
        authserv_id,
        results,
    })
}

/// One `method[/version]=result [reason=...] [ptype.property=value ...]`; `None` for `none`, an
/// empty segment, or anything malformed.
fn parse_result(tokens: &[Token]) -> Option<MethodResult> {
    let mut pairs = Vec::new();
    let mut key = String::new();
    let mut iter = tokens.iter();
    while let Some(token) = iter.next() {
        match token {
            Token::Word(w) | Token::Quoted(w) => key.push_str(w),
            Token::Equals => {
                let value = match iter.next()? {
                    Token::Word(v) | Token::Quoted(v) => v.clone(),
                    _ => return None,
                };
                if key.is_empty() {
                    return None;
                }
                pairs.push((std::mem::take(&mut key).to_ascii_lowercase(), value));
            }
            Token::Semicolon => return None,
        }
    }
    if !key.is_empty() {
        return None;
    }
    let mut pairs = pairs.into_iter();
    let (method, result) = pairs.next()?;
    let method = method.split('/').next().unwrap_or_default().to_owned();
    Some(MethodResult {
        method,
        result: result.to_ascii_lowercase(),
        properties: pairs.collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "mx.example.net";

    #[test]
    fn parses_methods_properties_comments_and_quotes() {
        let value = "mx.example.net 1; (a (nested) comment \\) still) \
                     dkim=pass (good signature) header.d=Example.com header.s=\"sel 1\"; \
                     spf = pass smtp.mailfrom=bounce@example.com; \
                     dmarc/1=fail reason=\"p=reject; adkim=s\" header.from=example.com; none";
        let parsed = parse(value).unwrap();
        assert_eq!(parsed.authserv_id, "mx.example.net");
        assert_eq!(
            parsed.results,
            vec![
                MethodResult {
                    method: "dkim".into(),
                    result: "pass".into(),
                    properties: vec![
                        ("header.d".into(), "Example.com".into()),
                        ("header.s".into(), "sel 1".into()),
                    ],
                },
                MethodResult {
                    method: "spf".into(),
                    result: "pass".into(),
                    properties: vec![("smtp.mailfrom".into(), "bounce@example.com".into())],
                },
                MethodResult {
                    method: "dmarc".into(),
                    result: "fail".into(),
                    properties: vec![
                        ("reason".into(), "p=reject; adkim=s".into()),
                        ("header.from".into(), "example.com".into()),
                    ],
                },
            ]
        );
    }

    #[test]
    fn unclosed_comments_and_quotes_fail_closed() {
        assert_eq!(parse("mx.example.net; dkim=pass (oops"), None);
        assert_eq!(parse("mx.example.net; dkim=pass header.d=\"x"), None);
        assert_eq!(parse("; dkim=pass"), None);
        assert_eq!(parse("a b c; dkim=pass"), None);
    }

    /// (case, topmost header value, From: domain, authenticated)
    const CASES: &[(&str, Option<&str>, &str, bool)] = &[
        ("no header", None, "example.com", false),
        (
            "dmarc pass",
            Some("mx.example.net; dmarc=pass header.from=example.com"),
            "example.com",
            true,
        ),
        (
            "dmarc pass, authserv-id case",
            Some("MX.Example.NET; dmarc=pass header.from=Example.COM"),
            "example.com",
            true,
        ),
        (
            "dmarc pass without header.from",
            Some("mx.example.net; dmarc=pass"),
            "example.com",
            false,
        ),
        (
            "dmarc pass for another domain",
            Some("mx.example.net; dmarc=pass header.from=evil.com"),
            "example.com",
            false,
        ),
        (
            "dmarc fail",
            Some("mx.example.net; dmarc=fail header.from=example.com"),
            "example.com",
            false,
        ),
        (
            "dmarc none",
            Some("mx.example.net; dmarc=none"),
            "example.com",
            false,
        ),
        (
            "authserv-id mismatch",
            Some("mx.evil.com; dmarc=pass"),
            "example.com",
            false,
        ),
        (
            "authserv-id is a prefix",
            Some("mx.example.net.evil.com; dmarc=pass"),
            "example.com",
            false,
        ),
        (
            "dkim pass, same domain",
            Some("mx.example.net; dkim=pass header.d=example.com"),
            "example.com",
            true,
        ),
        (
            "dkim pass, parent signer",
            Some("mx.example.net; dkim=pass header.d=example.com"),
            "mail.example.com",
            true,
        ),
        (
            "dkim pass, subdomain signer",
            Some("mx.example.net; dkim=pass header.d=mail.example.com"),
            "example.com",
            false,
        ),
        (
            "dkim pass, unaligned",
            Some("mx.example.net; dkim=pass header.d=evil.com"),
            "example.com",
            false,
        ),
        (
            "dkim pass, suffix but not a subdomain",
            Some("mx.example.net; dkim=pass header.d=ample.com"),
            "example.com",
            false,
        ),
        (
            "dkim pass, public suffix signer",
            Some("mx.example.net; dkim=pass header.d=com"),
            "example.com",
            false,
        ),
        (
            "dkim pass via header.i",
            Some("mx.example.net; dkim=pass header.i=@example.com header.s=s1"),
            "example.com",
            true,
        ),
        (
            "spaces around = and .",
            Some("mx.example.net ; dkim = pass header . d = example.com"),
            "example.com",
            true,
        ),
        (
            "dkim fail, aligned",
            Some("mx.example.net; dkim=fail header.d=example.com"),
            "example.com",
            false,
        ),
        (
            "dkim neutral",
            Some("mx.example.net; dkim=neutral header.d=example.com"),
            "example.com",
            false,
        ),
        (
            "spf pass only",
            Some("mx.example.net; spf=pass smtp.mailfrom=example.com"),
            "example.com",
            false,
        ),
        (
            "one of several dkim results aligned",
            Some("mx.example.net; dkim=pass header.d=esp.net; dkim=pass header.d=example.com"),
            "example.com",
            true,
        ),
        (
            "pass hidden in a comment",
            Some("mx.example.net; dkim=fail (dkim=pass header.d=example.com) header.d=example.com"),
            "example.com",
            false,
        ),
        (
            "pass hidden in a quoted reason",
            Some("mx.example.net; dmarc=fail reason=\"dmarc=pass\""),
            "example.com",
            false,
        ),
        (
            "quoted domain",
            Some("mx.example.net; dkim=pass header.d=\"Example.COM.\""),
            "example.com",
            true,
        ),
        (
            "no result",
            Some("mx.example.net; none"),
            "example.com",
            false,
        ),
        (
            "unparseable",
            Some("mx.example.net; dkim=pass (unclosed"),
            "example.com",
            false,
        ),
    ];

    #[test]
    fn verification_table() {
        for (case, value, from_domain, expected) in CASES {
            let verdict = verify(*value, ID, from_domain);
            assert_eq!(verdict.is_authenticated(), *expected, "{case}: {verdict:?}");
        }
    }
}
