use std::net::Ipv6Addr;

use super::{DEFAULT_PORT, ProtocolError};

/// Maximum URI length.
pub const MAX_URI_LEN: usize = 2048;

/// A parsed ATP URI: `atp://host[:port]/path[?query]`
#[derive(Debug, PartialEq, Eq)]
pub struct AtpUri {
    host: String,
    port: u16,
    path: String,
    query: Option<String>,
}

/// The scheme of `reference`, if it is an absolute URI in some other protocol.
///
/// RFC 3986's rule for telling an absolute URI from a relative reference: a
/// scheme is `ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )` followed by `:`,
/// before any `/`. Checking for `://` alone is not enough — `mailto:someone`
/// has no slashes and would otherwise be resolved as a filename.
///
/// The position of the colon relative to the first `/` is what keeps an ordinary
/// path safe: `sub/odd:name.aml` has its colon after a slash, so it is a path.
fn foreign_scheme(reference: &str) -> Option<&str> {
    let colon = reference.find(':')?;
    if reference[..colon].contains('/') {
        return None;
    }
    let scheme = &reference[..colon];
    let mut chars = scheme.chars();
    let first = chars.next()?;
    (first.is_ascii_alphabetic()
        && chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.')))
    .then_some(scheme)
}

impl AtpUri {
    #[cfg(test)]
    pub(crate) fn for_test(host: &str, port: u16, path: &str, query: Option<&str>) -> Self {
        Self {
            host: host.to_owned(),
            port,
            path: path.to_owned(),
            query: query.map(str::to_owned),
        }
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub const fn port(&self) -> u16 {
        self.port
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn query(&self) -> Option<&str> {
        self.query.as_deref()
    }

    pub fn try_path_query(
        &self,
    ) -> Result<(String, Option<String>), std::collections::TryReserveError> {
        fn owned(value: &str) -> Result<String, std::collections::TryReserveError> {
            let mut result = String::new();
            result.try_reserve_exact(value.len())?;
            result.push_str(value);
            Ok(result)
        }
        Ok((
            owned(&self.path)?,
            self.query.as_deref().map(owned).transpose()?,
        ))
    }

    /// Fallibly duplicate a parsed URI for retained operation state.
    pub fn try_clone(&self) -> Result<Self, std::collections::TryReserveError> {
        fn owned(value: &str) -> Result<String, std::collections::TryReserveError> {
            let mut result = String::new();
            result.try_reserve_exact(value.len())?;
            result.push_str(value);
            Ok(result)
        }

        Ok(Self {
            host: owned(&self.host)?,
            port: self.port,
            path: owned(&self.path)?,
            query: self.query.as_deref().map(owned).transpose()?,
        })
    }

    fn try_owned(value: &str, site: UriAllocationSite) -> Result<String, ProtocolError> {
        reject_uri_allocation(site, value.len())?;
        let mut owned = String::new();
        owned
            .try_reserve_exact(value.len())
            .map_err(|_| ProtocolError::ResourceExhausted {
                requested: value.len(),
            })?;
        owned.push_str(value);
        Ok(owned)
    }

    /// Parse an ATP URI string.
    ///
    /// Accepts `atp://host[:port]/path`. Port defaults to 1985.
    /// Path defaults to `/` if omitted.
    pub fn parse(s: &str) -> Result<Self, ProtocolError> {
        if s.len() > MAX_URI_LEN {
            return Err(ProtocolError::invalid_uri(format_args!(
                "URI exceeds maximum length of {MAX_URI_LEN}"
            )));
        }

        if s.chars().any(char::is_control) || s.contains('#') {
            return Err(ProtocolError::InvalidUri(
                "control characters and fragments are not allowed".into(),
            ));
        }

        if !s.starts_with("atp://") {
            return Err(ProtocolError::invalid_uri(format_args!(
                "expected atp:// scheme, got: {}",
                s.split_once("://").map(|(s, _)| s).unwrap_or(s)
            )));
        }

        let after_scheme = &s[6..]; // skip "atp://"
        if after_scheme.is_empty() {
            return Err(ProtocolError::InvalidUri("missing host".into()));
        }

        // Split host[:port] from path[?query]
        let authority_end = after_scheme.find(['/', '?']).unwrap_or(after_scheme.len());
        let host_port = &after_scheme[..authority_end];
        let path_and_query = if authority_end == after_scheme.len() {
            "/"
        } else {
            &after_scheme[authority_end..]
        };

        // Split path from query string
        let (path, query) = if let Some(query) = path_and_query.strip_prefix('?') {
            ("/", Some(Self::try_owned(query, UriAllocationSite::Query)?))
        } else {
            match path_and_query.find('?') {
                Some(idx) => (
                    &path_and_query[..idx],
                    Some(Self::try_owned(
                        &path_and_query[idx + 1..],
                        UriAllocationSite::Query,
                    )?),
                ),
                None => (path_and_query, None),
            }
        };
        let query = query.filter(|q| !q.is_empty());

        if host_port.is_empty() {
            return Err(ProtocolError::InvalidUri("missing host".into()));
        }

        if host_port
            .chars()
            .any(|c| c.is_control() || c.is_whitespace())
            || host_port.contains(['@', '#', '?'])
        {
            return Err(ProtocolError::InvalidUri("invalid host".into()));
        }

        // Split host and port. Brackets are required for IPv6 literals so a
        // colon in the address can never be confused with the port separator.
        let (host, port) = if let Some(rest) = host_port.strip_prefix('[') {
            let end = rest
                .find(']')
                .ok_or_else(|| ProtocolError::InvalidUri("unterminated IPv6 address".into()))?;
            let host = &rest[..end];
            host.parse::<Ipv6Addr>()
                .map_err(|_| ProtocolError::InvalidUri("invalid IPv6 address".into()))?;
            let suffix = &rest[end + 1..];
            let port = if suffix.is_empty() {
                DEFAULT_PORT
            } else {
                let value = suffix
                    .strip_prefix(':')
                    .ok_or_else(|| ProtocolError::InvalidUri("invalid IPv6 authority".into()))?;
                parse_port(value)?
            };
            (host, port)
        } else if let Some((host, port_str)) = host_port.rsplit_once(':') {
            if host.contains(':') {
                return Err(ProtocolError::InvalidUri(
                    "IPv6 addresses must be enclosed in brackets".into(),
                ));
            }
            (host, parse_port(port_str)?)
        } else {
            (host_port, DEFAULT_PORT)
        };

        if host.is_empty() {
            return Err(ProtocolError::InvalidUri("missing host".into()));
        }
        if !host.is_ascii() {
            return Err(ProtocolError::InvalidUri(
                "host must be an ASCII DNS name or IP literal".into(),
            ));
        }

        reject_uri_allocation(UriAllocationSite::Host, host.len())?;
        let mut canonical_host = String::new();
        canonical_host.try_reserve_exact(host.len()).map_err(|_| {
            ProtocolError::ResourceExhausted {
                requested: host.len(),
            }
        })?;
        canonical_host.extend(host.bytes().map(|byte| (byte.to_ascii_lowercase()) as char));

        Ok(AtpUri {
            host: canonical_host,
            port,
            path: try_normalize_path(path, "")?,
            query,
        })
    }

    /// Resolve a relative path against this URI.
    ///
    /// - Absolute URI (`atp://...`) → returns parsed absolute URI
    /// - Absolute path (`/foo`) → same host:port, new path
    /// - Relative path (`bar`) → resolve against current path's directory
    pub fn resolve(&self, relative: &str) -> Result<AtpUri, ProtocolError> {
        // (see `foreign_scheme` below for why a scheme check comes first)
        if relative.len() > MAX_URI_LEN {
            return Err(ProtocolError::invalid_uri(format_args!(
                "URI exceeds maximum length of {MAX_URI_LEN}"
            )));
        }
        if relative.chars().any(char::is_control) || relative.contains('#') {
            return Err(ProtocolError::InvalidUri(
                "control characters and fragments are not allowed".into(),
            ));
        }

        // If it's an absolute URI, just parse it
        if relative.starts_with("atp://") {
            return AtpUri::parse(relative);
        }

        // Any other scheme is an absolute URI this protocol cannot follow, and
        // must not be mistaken for a path. Without this an `https://example.com`
        // href resolved as a *relative* reference and became
        // `atp://current-host/https://example.com`: a nonsensical request, and
        // one that tells the current site which external link was clicked.
        //
        // The scheme is named in the error so a caller can say which protocol it
        // refused rather than reporting a generic failure.
        if let Some(scheme) = foreign_scheme(relative) {
            return Err(ProtocolError::invalid_uri(format_args!(
                "unsupported protocol: {scheme}"
            )));
        }

        // Split off query string from the href
        let (rel_path, query) = match relative.find('?') {
            Some(idx) => (
                &relative[..idx],
                Some(Self::try_owned(
                    &relative[idx + 1..],
                    UriAllocationSite::Query,
                )?),
            ),
            None => (relative, None),
        };
        let query = query.filter(|q| !q.is_empty());

        if rel_path.starts_with('/') {
            // Absolute path
            return Ok(AtpUri {
                host: Self::try_owned(&self.host, UriAllocationSite::Host)?,
                port: self.port,
                path: try_normalize_path(rel_path, "")?,
                query,
            });
        }

        if rel_path.is_empty() {
            return Ok(AtpUri {
                host: Self::try_owned(&self.host, UriAllocationSite::Host)?,
                port: self.port,
                path: Self::try_owned(&self.path, UriAllocationSite::Path)?,
                query,
            });
        }

        // Relative path — resolve against current path's directory
        let base_dir = match self.path.rfind('/') {
            Some(idx) => &self.path[..=idx],
            None => "/",
        };

        Ok(AtpUri {
            host: Self::try_owned(&self.host, UriAllocationSite::Host)?,
            port: self.port,
            path: try_normalize_path(base_dir, rel_path)?,
            query,
        })
    }
}

impl std::fmt::Display for AtpUri {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.host.contains(':') {
            write!(f, "atp://[{}]", self.host)?;
        } else {
            write!(f, "atp://{}", self.host)?;
        }
        if self.port == DEFAULT_PORT {
            write!(f, "{}", self.path)?;
        } else {
            write!(f, ":{}{}", self.port, self.path)?;
        }
        if let Some(ref q) = self.query {
            write!(f, "?{q}")?;
        }
        Ok(())
    }
}

fn parse_port(value: &str) -> Result<u16, ProtocolError> {
    if value.is_empty() {
        return Err(ProtocolError::InvalidUri("empty port".into()));
    }
    value
        .parse::<u16>()
        .map_err(|_| ProtocolError::invalid_uri(format_args!("invalid port: {value}")))
}

/// Normalize a path by resolving `.` and `..` segments.
fn try_normalize_path(first: &str, second: &str) -> Result<String, ProtocolError> {
    let final_part = if second.is_empty() { first } else { second };
    let trailing_slash = final_part.len() > 1 && final_part.ends_with('/');
    let requested = first
        .len()
        .checked_add(second.len())
        .ok_or(ProtocolError::ResourceExhausted {
            requested: usize::MAX,
        })?
        .max(1);
    if requested > MAX_URI_LEN {
        return Err(ProtocolError::InvalidUri(
            "resolved URI path exceeds maximum length".into(),
        ));
    }
    reject_uri_allocation(UriAllocationSite::Path, requested)?;
    let mut normalized = String::new();
    normalized
        .try_reserve_exact(requested)
        .map_err(|_| ProtocolError::ResourceExhausted { requested })?;
    normalized.push('/');
    for segment in first.split('/').chain(second.split('/')) {
        match segment {
            "" | "." => {}
            ".." => {
                if normalized.len() > 1 {
                    normalized.pop();
                    normalized.truncate(normalized.rfind('/').map_or(1, |index| index.max(1)));
                }
            }
            value => {
                if normalized.len() > 1 && !normalized.ends_with('/') {
                    normalized.push('/');
                }
                normalized.push_str(value);
            }
        }
    }
    if trailing_slash && normalized != "/" && !normalized.ends_with('/') {
        normalized.push('/');
    }
    Ok(normalized)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum UriAllocationSite {
    Host,
    Path,
    Query,
}

fn reject_uri_allocation(site: UriAllocationSite, requested: usize) -> Result<(), ProtocolError> {
    #[cfg(test)]
    if REJECT_URI_ALLOCATION.with(|reject| {
        if reject.get() == Some(site) {
            reject.set(None);
            true
        } else {
            false
        }
    }) {
        return Err(ProtocolError::ResourceExhausted { requested });
    }
    let _ = site;
    let _ = requested;
    Ok(())
}

#[cfg(test)]
thread_local! {
    static REJECT_URI_ALLOCATION: std::cell::Cell<Option<UriAllocationSite>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallible_clone_preserves_complete_uri() {
        let uri = AtpUri::parse("atp://example.com:2000/path?q=1").unwrap();
        assert_eq!(uri.try_clone().unwrap(), uri);
    }

    /// Deliberately separate, test-only normalization model. It constructs the
    /// result from owned components rather than borrowing the implementation's
    /// segment stack, and is exercised over a Cartesian input space below.
    fn reference_normalize_path(path: &str) -> String {
        let preserve_trailing_slash = path.len() > 1 && path.as_bytes().last() == Some(&b'/');
        let components = path
            .split('/')
            .fold(Vec::<String>::new(), |mut result, part| {
                if part == ".." {
                    result.truncate(result.len().saturating_sub(1));
                } else if !part.is_empty() && part != "." {
                    result.push(part.to_owned());
                }
                result
            });
        let mut result = String::from("/");
        result.push_str(&components.join("/"));
        if preserve_trailing_slash && result != "/" {
            result.push('/');
        }
        result
    }

    #[test]
    fn parse_full_uri() {
        let uri = AtpUri::parse("atp://example.com:2000/hello").unwrap();
        assert_eq!(uri.host, "example.com");
        assert_eq!(uri.port, 2000);
        assert_eq!(uri.path, "/hello");
    }

    #[test]
    fn canonicalizes_host_and_absolute_path() {
        let uri = AtpUri::parse("atp://EXAMPLE.COM/a/../b/").unwrap();
        assert_eq!(uri.host, "example.com");
        assert_eq!(uri.path, "/b/");
    }

    #[test]
    fn rejects_invalid_port_and_header_injection() {
        assert!(AtpUri::parse("atp://example.com:nope/").is_err());
        assert!(AtpUri::parse("atp://example.com/path\nSession: stolen").is_err());
    }

    #[test]
    fn parses_bracketed_ipv6() {
        let uri = AtpUri::parse("atp://[::1]:2000/hello").unwrap();
        assert_eq!(uri.host, "::1");
        assert_eq!(uri.port, 2000);
        assert_eq!(uri.to_string(), "atp://[::1]:2000/hello");
    }

    #[test]
    fn rejects_invalid_bracketed_ipv6() {
        assert!(AtpUri::parse("atp://[not-an-ip]/").is_err());
    }

    #[test]
    fn relative_resolution_applies_uri_validation() {
        let base = AtpUri::parse("atp://example.com/a/page").unwrap();
        assert_eq!(base.resolve("?mode=full").unwrap().path, "/a/page");
        assert_eq!(base.resolve("/x/../y").unwrap().path, "/y");
        assert!(base.resolve("/x#fragment").is_err());
        assert!(base.resolve("/x\r\nInjected: yes").is_err());
    }

    #[test]
    fn parses_query_without_explicit_path() {
        let uri = AtpUri::parse("atp://example.com?item=3").unwrap();
        assert_eq!(uri.path, "/");
        assert_eq!(uri.query.as_deref(), Some("item=3"));
    }

    #[test]
    fn parse_default_port() {
        let uri = AtpUri::parse("atp://example.com/hello").unwrap();
        assert_eq!(uri.host, "example.com");
        assert_eq!(uri.port, DEFAULT_PORT);
        assert_eq!(uri.path, "/hello");
    }

    #[test]
    fn parse_no_path() {
        let uri = AtpUri::parse("atp://example.com").unwrap();
        assert_eq!(uri.host, "example.com");
        assert_eq!(uri.port, DEFAULT_PORT);
        assert_eq!(uri.path, "/");
    }

    #[test]
    fn parse_root_path() {
        let uri = AtpUri::parse("atp://example.com/").unwrap();
        assert_eq!(uri.path, "/");
    }

    #[test]
    fn parse_deep_path() {
        let uri = AtpUri::parse("atp://example.com/a/b/c").unwrap();
        assert_eq!(uri.path, "/a/b/c");
    }

    /// An absolute URI in another protocol must be refused, not resolved as a
    /// relative path. Resolving it produced a request to the current host with
    /// the foreign URI as its path.
    #[test]
    fn resolve_refuses_other_protocols_rather_than_treating_them_as_paths() {
        let base = AtpUri::parse("atp://news.dustnet.io/index.aml").unwrap();
        for href in [
            "https://example.com/article",
            "http://example.com",
            "mailto:rob@dustnet.io",
            "file:///etc/passwd",
        ] {
            let error = base
                .resolve(href)
                .expect_err(&format!("resolved {href} instead of refusing it"));
            let message = format!("{error}");
            assert!(
                message.contains("unsupported protocol"),
                "{href}: {message}"
            );
        }
    }

    /// A path that merely contains a colon is still a path.
    #[test]
    fn resolve_still_accepts_ordinary_relative_references() {
        let base = AtpUri::parse("atp://news.dustnet.io/index.aml").unwrap();
        for href in [
            "about.aml",
            "/index?item=1",
            "sub/page.aml",
            "?item=2",
            // A colon after a slash is part of a path, not a scheme.
            "sub/odd:name.aml",
        ] {
            assert!(base.resolve(href).is_ok(), "refused {href}");
        }
    }

    #[test]
    fn rejects_http_scheme() {
        assert!(AtpUri::parse("http://example.com/foo").is_err());
    }

    #[test]
    fn rejects_https_scheme() {
        assert!(AtpUri::parse("https://example.com/foo").is_err());
    }

    #[test]
    fn rejects_no_scheme() {
        assert!(AtpUri::parse("example.com/foo").is_err());
    }

    #[test]
    fn rejects_empty() {
        assert!(AtpUri::parse("").is_err());
    }

    #[test]
    fn rejects_too_long() {
        let long = format!("atp://example.com/{}", "a".repeat(MAX_URI_LEN));
        assert!(AtpUri::parse(&long).is_err());
    }

    #[test]
    fn display_default_port() {
        let uri = AtpUri {
            host: "example.com".into(),
            port: DEFAULT_PORT,
            path: "/hello".into(),
            query: None,
        };
        assert_eq!(uri.to_string(), "atp://example.com/hello");
    }

    #[test]
    fn display_custom_port() {
        let uri = AtpUri {
            host: "example.com".into(),
            port: 2000,
            path: "/hello".into(),
            query: None,
        };
        assert_eq!(uri.to_string(), "atp://example.com:2000/hello");
    }

    // ─── Resolution ──────────────────────────────────────────

    #[test]
    fn resolve_absolute_uri() {
        let base = AtpUri::parse("atp://example.com/foo").unwrap();
        let resolved = base.resolve("atp://other.com/bar").unwrap();
        assert_eq!(resolved.host, "other.com");
        assert_eq!(resolved.path, "/bar");
    }

    #[test]
    fn resolve_absolute_path() {
        let base = AtpUri::parse("atp://example.com/foo/bar").unwrap();
        let resolved = base.resolve("/baz").unwrap();
        assert_eq!(resolved.host, "example.com");
        assert_eq!(resolved.path, "/baz");
    }

    #[test]
    fn resolve_relative_path() {
        let base = AtpUri::parse("atp://example.com/foo/bar").unwrap();
        let resolved = base.resolve("baz").unwrap();
        assert_eq!(resolved.host, "example.com");
        assert_eq!(resolved.path, "/foo/baz");
    }

    #[test]
    fn resolve_relative_with_dotdot() {
        let base = AtpUri::parse("atp://example.com/a/b/c").unwrap();
        let resolved = base.resolve("../d").unwrap();
        assert_eq!(resolved.path, "/a/d");
    }

    #[test]
    fn resolve_preserves_port() {
        let base = AtpUri::parse("atp://example.com:2000/foo").unwrap();
        let resolved = base.resolve("/bar").unwrap();
        assert_eq!(resolved.port, 2000);
    }

    #[test]
    fn normalize_path_basic() {
        assert_eq!(try_normalize_path("/a/b/../c", "").unwrap(), "/a/c");
        assert_eq!(try_normalize_path("/a/./b", "").unwrap(), "/a/b");
        assert_eq!(try_normalize_path("/a/b/c/../../d", "").unwrap(), "/a/d");
        assert_eq!(try_normalize_path("/", "").unwrap(), "/");
    }

    #[test]
    fn uri_allocation_rejection_is_recoverable() {
        for site in [
            UriAllocationSite::Host,
            UriAllocationSite::Path,
            UriAllocationSite::Query,
        ] {
            REJECT_URI_ALLOCATION.with(|reject| reject.set(Some(site)));
            assert!(matches!(
                AtpUri::parse("atp://example.com/path?q=1"),
                Err(ProtocolError::ResourceExhausted { .. })
            ));
        }

        let base = AtpUri::parse("atp://example.com/base/page").unwrap();
        REJECT_URI_ALLOCATION.with(|reject| reject.set(Some(UriAllocationSite::Query)));
        assert!(matches!(
            base.resolve("../next?q=1"),
            Err(ProtocolError::ResourceExhausted { .. })
        ));
    }

    #[test]
    fn relative_resolution_rejects_a_combined_path_over_the_field_bound() {
        let base_path = "a".repeat(MAX_URI_LEN - "atp://example.com//".len());
        let base = AtpUri::parse(&format!("atp://example.com/{base_path}/")).unwrap();
        let relative = "b".repeat(64);
        assert!(matches!(
            base.resolve(&relative),
            Err(ProtocolError::InvalidUri(_))
        ));
    }

    #[test]
    fn generated_paths_match_independent_normalization_model() {
        // 7^4 component combinations, each checked with and without a trailing
        // slash. Empty, dot, dot-dot, and ordinary segments exercise root
        // clamping, repeated separators, and retained path data.
        const PARTS: [&str; 7] = ["", ".", "..", "a", "b", "c-d", "x_y"];
        for first in PARTS {
            for second in PARTS {
                for third in PARTS {
                    for fourth in PARTS {
                        let body = [first, second, third, fourth].join("/");
                        for suffix in ["", "/"] {
                            let input = format!("/{body}{suffix}");
                            let expected = reference_normalize_path(&input);
                            let parsed = AtpUri::parse(&format!(
                                "atp://MiXeD.Example{input}?case={}",
                                input.len()
                            ))
                            .unwrap();

                            assert_eq!(parsed.host, "mixed.example", "input: {input}");
                            assert_eq!(parsed.path, expected, "input: {input}");

                            // Canonical display is a fixed point of parsing.
                            let reparsed = AtpUri::parse(&parsed.to_string()).unwrap();
                            assert_eq!(reparsed, parsed, "input: {input}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn generated_host_case_variants_have_one_canonical_uri() {
        let letters = b"example";
        for mask in 0_u8..(1 << letters.len()) {
            let host: String = letters
                .iter()
                .enumerate()
                .map(|(index, byte)| {
                    if mask & (1 << index) == 0 {
                        char::from(*byte)
                    } else {
                        char::from(byte.to_ascii_uppercase())
                    }
                })
                .collect();
            let parsed = AtpUri::parse(&format!("atp://{host}.COM/a/./b/../c")).unwrap();
            assert_eq!(parsed.to_string(), "atp://example.com/a/c");
        }
    }

    // ─── Query strings ──────────────────────────────────────

    #[test]
    fn parse_with_query() {
        let uri = AtpUri::parse("atp://example.com/links?item=3").unwrap();
        assert_eq!(uri.path, "/links");
        assert_eq!(uri.query, Some("item=3".into()));
    }

    #[test]
    fn parse_empty_query_is_none() {
        let uri = AtpUri::parse("atp://example.com/links?").unwrap();
        assert_eq!(uri.path, "/links");
        assert_eq!(uri.query, None);
    }

    #[test]
    fn resolve_absolute_path_with_query() {
        let base = AtpUri::parse("atp://example.com/foo").unwrap();
        let resolved = base.resolve("/links?item=3").unwrap();
        assert_eq!(resolved.path, "/links");
        assert_eq!(resolved.query, Some("item=3".into()));
    }

    #[test]
    fn resolve_relative_with_query() {
        let base = AtpUri::parse("atp://example.com/dir/page").unwrap();
        let resolved = base.resolve("links?vote=5").unwrap();
        assert_eq!(resolved.path, "/dir/links");
        assert_eq!(resolved.query, Some("vote=5".into()));
    }

    #[test]
    fn display_with_query() {
        let uri = AtpUri {
            host: "example.com".into(),
            port: DEFAULT_PORT,
            path: "/links".into(),
            query: Some("item=3".into()),
        };
        assert_eq!(uri.to_string(), "atp://example.com/links?item=3");
    }
}
