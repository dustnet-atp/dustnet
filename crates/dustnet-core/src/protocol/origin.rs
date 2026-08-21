use std::cmp::Ordering;
use std::fmt::{self, Write as _};
use std::net::IpAddr;

use super::{
    ProtocolError,
    uri::{AtpUri, MAX_URI_LEN},
};

/// The security properties of the transport that established an ATP origin.
///
/// Security context is part of origin identity. Content learned over an
/// insecure connection must never be reused by a verified TLS connection (or
/// vice versa), even when host and port are identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransportSecurity {
    /// A certification authority vouched for this host name.
    VerifiedTls,
    /// The certificate matched one this user pinned for this host and port,
    /// and the handshake signature was verified against it. Authenticated,
    /// but by a decision this user made rather than by a shared authority —
    /// so state learned here must not be reused by a CA-verified connection,
    /// which is a different claim about who the peer is.
    PinnedTls,
    /// The certificate was not checked at all.
    InsecureTls,
    PlaintextLoopback,
}

impl TransportSecurity {
    /// Every level, so a matrix test cannot quietly stop covering one.
    ///
    /// A new variant is caught by [`Origin::security_label`], which is an
    /// exhaustive match and will not compile until the variant is handled;
    /// this list is what the tests that sweep the whole space iterate, and
    /// extending it is the second half of that change.
    pub const ALL: [Self; 4] = [
        Self::VerifiedTls,
        Self::PinnedTls,
        Self::InsecureTls,
        Self::PlaintextLoopback,
    ];
}

/// Canonical authority and transport security for security-sensitive state.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Origin {
    host: String,
    port: u16,
    security: TransportSecurity,
}

impl Origin {
    pub fn from_uri(uri: &AtpUri, security: TransportSecurity) -> Result<Self, ProtocolError> {
        let host = if uri.host().is_ascii() {
            try_canonical_host(uri.host())?
        } else {
            canonical_host(uri.host())?
        };
        Self::from_canonical_host(uri, security, host)
    }

    /// Fallibly derive a canonical origin from a parsed production ATP URI.
    ///
    /// `AtpUri::parse` admits only bounded ASCII hosts. Keeping this separate
    /// from the Unicode-IDNA compatibility path makes every successful host
    /// allocation recoverable before client connection state is mutated.
    pub fn try_from_uri(uri: &AtpUri, security: TransportSecurity) -> Result<Self, ProtocolError> {
        let host = try_canonical_host(uri.host())?;
        Self::from_canonical_host(uri, security, host)
    }

    fn from_canonical_host(
        uri: &AtpUri,
        security: TransportSecurity,
        host: String,
    ) -> Result<Self, ProtocolError> {
        if security == TransportSecurity::PlaintextLoopback && !is_loopback_host(&host) {
            return Err(ProtocolError::InvalidUri(
                "plaintext ATP is restricted to loopback hosts".into(),
            ));
        }
        Ok(Self {
            host,
            port: uri.port(),
            security,
        })
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    /// Heap capacity retained by the canonical host string.
    ///
    /// Resource-owning clients use this instead of `len()` when charging
    /// remotely influenced collection storage.
    pub fn host_capacity(&self) -> usize {
        self.host.capacity()
    }

    /// Fallibly copy this origin for retained, remotely influenced stores.
    pub fn try_clone(&self) -> Result<Self, std::collections::TryReserveError> {
        let mut host = String::new();
        host.try_reserve_exact(self.host.len())?;
        host.push_str(&self.host);
        Ok(Self {
            host,
            port: self.port,
            security: self.security,
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn security(&self) -> TransportSecurity {
        self.security
    }

    /// Stable key for stores that cannot yet use `Origin` directly.
    pub fn storage_key(&self) -> Result<String, ProtocolError> {
        let requested = self
            .security_label()
            .len()
            .saturating_add(1)
            .saturating_add(self.host.len())
            .saturating_add(1)
            .saturating_add(5);
        let mut key = String::new();
        #[cfg(test)]
        if reject_storage_key_allocation() {
            return Err(ProtocolError::ResourceExhausted { requested });
        }
        key.try_reserve_exact(requested)
            .map_err(|_| ProtocolError::ResourceExhausted { requested })?;
        let _ = write!(&mut key, "{}", self.storage_key_display());
        Ok(key)
    }

    /// Allocation-free formatter for the stable storage key.
    pub fn storage_key_display(&self) -> OriginStorageKey<'_> {
        OriginStorageKey(self)
    }

    /// Compare a local UI storage key without allocating a duplicate key.
    pub fn matches_storage_key(&self, key: &str) -> bool {
        let mut port = [0; 5];
        let start = write_decimal_u16(self.port, &mut port);
        self.security_label()
            .bytes()
            .chain(std::iter::once(b'|'))
            .chain(self.host.bytes())
            .chain(std::iter::once(b':'))
            .chain(port.get(start..).unwrap_or(&[]).iter().copied())
            .eq(key.bytes())
    }

    /// Compare stable storage keys without allocating either formatted key.
    pub fn cmp_storage_key(&self, other: &Self) -> Ordering {
        let mut left_port = [0; 5];
        let mut right_port = [0; 5];
        let left_start = write_decimal_u16(self.port, &mut left_port);
        let right_start = write_decimal_u16(other.port, &mut right_port);
        self.security_label()
            .bytes()
            .chain(std::iter::once(b'|'))
            .chain(self.host.bytes())
            .chain(std::iter::once(b':'))
            .chain(left_port.get(left_start..).unwrap_or(&[]).iter().copied())
            .cmp(
                other
                    .security_label()
                    .bytes()
                    .chain(std::iter::once(b'|'))
                    .chain(other.host.bytes())
                    .chain(std::iter::once(b':'))
                    .chain(right_port.get(right_start..).unwrap_or(&[]).iter().copied()),
            )
    }

    fn security_label(&self) -> &'static str {
        match self.security {
            TransportSecurity::VerifiedTls => "verified-tls",
            TransportSecurity::PinnedTls => "pinned-tls",
            TransportSecurity::InsecureTls => "insecure-tls",
            TransportSecurity::PlaintextLoopback => "plaintext-loopback",
        }
    }
}

fn write_decimal_u16(mut value: u16, digits: &mut [u8; 5]) -> usize {
    let mut start = digits.len();
    loop {
        start -= 1;
        // `u16::MAX` is five digits, so the loop cannot outrun the buffer.
        // Writing through `get_mut` keeps that checked rather than resting on
        // an argument about the type's range.
        if let Some(slot) = digits.get_mut(start) {
            *slot = b'0' + (value % 10) as u8;
        }
        value /= 10;
        if value == 0 {
            return start;
        }
    }
}

#[cfg(test)]
thread_local! {
    static REJECT_STORAGE_KEY_ALLOCATION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn reject_storage_key_allocation() -> bool {
    REJECT_STORAGE_KEY_ALLOCATION.with(|reject| reject.replace(false))
}

/// Borrowed allocation-free display form of an [`Origin`] storage key.
pub struct OriginStorageKey<'a>(&'a Origin);

impl fmt::Display for OriginStorageKey<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}|{}:{}",
            self.0.security_label(),
            self.0.host,
            self.0.port
        )
    }
}

fn try_canonical_host(host: &str) -> Result<String, ProtocolError> {
    let unbracketed = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host);
    if !unbracketed.is_ascii() || unbracketed.len() > MAX_URI_LEN {
        return Err(ProtocolError::InvalidUri(
            "origin host must be a bounded ASCII name or IP literal".into(),
        ));
    }
    if let Ok(address) = unbracketed.parse::<IpAddr>() {
        let requested = 64usize;
        let mut canonical = String::new();
        canonical
            .try_reserve_exact(requested)
            .map_err(|_| ProtocolError::ResourceExhausted { requested })?;
        write!(&mut canonical, "{address}")
            .map_err(|_| ProtocolError::ResourceExhausted { requested })?;
        return Ok(canonical);
    }
    let dns = unbracketed.trim_end_matches('.');
    if dns.is_empty() {
        return Err(ProtocolError::InvalidUri("missing DNS name".into()));
    }
    let requested = dns.len();
    let mut canonical = String::new();
    canonical
        .try_reserve_exact(requested)
        .map_err(|_| ProtocolError::ResourceExhausted { requested })?;
    canonical.extend(
        dns.bytes()
            .map(|byte| char::from(byte.to_ascii_lowercase())),
    );
    match idna::domain_to_ascii_cow(canonical.as_bytes(), idna::AsciiDenyList::EMPTY) {
        Ok(std::borrow::Cow::Borrowed(validated)) if validated == canonical => Ok(canonical),
        Ok(_) | Err(_) => Err(ProtocolError::InvalidUri("invalid IDNA DNS name".into())),
    }
}

fn canonical_host(host: &str) -> Result<String, ProtocolError> {
    let unbracketed = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(address) = unbracketed.parse::<IpAddr>() {
        return Ok(address.to_string());
    }
    let dns = unbracketed.trim_end_matches('.');
    if dns.is_empty() {
        return Err(ProtocolError::InvalidUri("missing DNS name".into()));
    }
    idna::domain_to_ascii(dns)
        .map(|name| name.to_ascii_lowercase())
        .map_err(|_| ProtocolError::InvalidUri("invalid IDNA DNS name".into()))
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::DEFAULT_PORT;
    use std::collections::HashSet;

    fn uri_with_host(host: &str, port: u16) -> AtpUri {
        AtpUri::for_test(host, port, "/ignored/by/origin", Some("also-ignored=true"))
    }

    #[test]
    fn security_context_partitions_otherwise_equal_origins() {
        let uri = AtpUri::parse("atp://EXAMPLE.com/").unwrap();
        let verified = Origin::from_uri(&uri, TransportSecurity::VerifiedTls).unwrap();
        let insecure = Origin::from_uri(&uri, TransportSecurity::InsecureTls).unwrap();
        assert_ne!(verified, insecure);
        assert_ne!(
            verified.storage_key().unwrap(),
            insecure.storage_key().unwrap()
        );

        // Every pair, not just the two that existed when this was written: a
        // new security level that collided with an existing one would let
        // state cross a trust boundary, and pairwise is the only way to say
        // that without naming the levels. Loopback, because plaintext is the
        // one level that cannot be constructed for any other host.
        let local = AtpUri::parse("atp://localhost/").unwrap();
        let origins: Vec<Origin> = TransportSecurity::ALL
            .into_iter()
            .map(|security| Origin::from_uri(&local, security).unwrap())
            .collect();
        for (index, left) in origins.iter().enumerate() {
            for right in &origins[index + 1..] {
                assert_ne!(left, right, "{left:?} and {right:?} share an identity");
                assert_ne!(
                    left.storage_key().unwrap(),
                    right.storage_key().unwrap(),
                    "{left:?} and {right:?} share a storage key"
                );
            }
        }
    }

    #[test]
    fn storage_keys_format_compare_and_match_without_duplicate_keys() {
        let ten = Origin::from_uri(
            &AtpUri::parse("atp://example.com:10/").unwrap(),
            TransportSecurity::VerifiedTls,
        )
        .unwrap();
        let two = Origin::from_uri(
            &AtpUri::parse("atp://example.com:2/").unwrap(),
            TransportSecurity::VerifiedTls,
        )
        .unwrap();
        let ipv6 = Origin::from_uri(
            &AtpUri::parse("atp://[::1]:1985/").unwrap(),
            TransportSecurity::VerifiedTls,
        )
        .unwrap();

        let key = ipv6.storage_key().unwrap();
        assert_eq!(key, ipv6.storage_key_display().to_string());
        assert!(ipv6.matches_storage_key(&key));
        assert!(!ipv6.matches_storage_key("verified-tls|::1:1986"));
        assert!(!ipv6.matches_storage_key("verified-tls|::1:01985"));
        assert!(!ipv6.matches_storage_key("verified-tls|::1:+1985"));
        assert_eq!(ten.cmp_storage_key(&two), "10".cmp("2"));

        let prefix = Origin::from_uri(
            &AtpUri::parse("atp://a:10/").unwrap(),
            TransportSecurity::VerifiedTls,
        )
        .unwrap();
        let extended = Origin::from_uri(
            &AtpUri::parse("atp://a-b:2/").unwrap(),
            TransportSecurity::VerifiedTls,
        )
        .unwrap();
        assert_eq!(
            prefix.cmp_storage_key(&extended),
            prefix
                .storage_key()
                .unwrap()
                .cmp(&extended.storage_key().unwrap())
        );

        REJECT_STORAGE_KEY_ALLOCATION.with(|reject| reject.set(true));
        assert!(matches!(
            ipv6.storage_key(),
            Err(ProtocolError::ResourceExhausted { .. })
        ));
    }

    #[test]
    fn fallible_production_derivation_matches_canonical_ascii_origins() {
        for value in [
            "atp://EXAMPLE.com./",
            "atp://127.0.0.1/",
            "atp://[0:0:0:0:0:0:0:1]/",
            "atp://xn--bcher-kva.example/",
        ] {
            let uri = AtpUri::parse(value).unwrap();
            assert_eq!(
                Origin::try_from_uri(&uri, TransportSecurity::VerifiedTls).unwrap(),
                Origin::from_uri(&uri, TransportSecurity::VerifiedTls).unwrap(),
            );
        }

        let non_production = AtpUri::for_test("bücher.example", DEFAULT_PORT, "/", None);
        assert!(matches!(
            Origin::try_from_uri(&non_production, TransportSecurity::VerifiedTls),
            Err(ProtocolError::InvalidUri(_)),
        ));
    }

    #[test]
    fn canonicalizes_idna_trailing_dot_and_ip_literals() {
        let unicode = AtpUri::for_test("BÜCHER.example.", 1985, "/", None);
        let origin = Origin::from_uri(&unicode, TransportSecurity::VerifiedTls).unwrap();
        assert_eq!(origin.host(), "xn--bcher-kva.example");

        let ipv6 = AtpUri::for_test("0:0:0:0:0:0:0:1", 1985, "/", None);
        let origin = Origin::from_uri(&ipv6, TransportSecurity::PlaintextLoopback).unwrap();
        assert_eq!(origin.host(), "::1");
    }

    #[test]
    fn plaintext_is_loopback_only() {
        let remote = AtpUri::parse("atp://example.com/").unwrap();
        assert!(Origin::from_uri(&remote, TransportSecurity::PlaintextLoopback).is_err());
        for host in ["localhost", "127.0.0.1", "[::1]"] {
            let uri = AtpUri::parse(&format!("atp://{host}/")).unwrap();
            assert!(Origin::from_uri(&uri, TransportSecurity::PlaintextLoopback).is_ok());
        }
    }

    #[test]
    fn generated_origin_identity_matches_reference_partition_tuple() {
        // Each alias group has an independently stated canonical host. The
        // pairwise assertion below proves that equality and storage keys vary
        // exactly with canonical host, port, and transport security—not URI
        // path/query or input spelling.
        let alias_groups: [(&str, &[&str], bool); 4] = [
            (
                "example.com",
                &["example.com", "EXAMPLE.COM", "Example.Com."],
                false,
            ),
            (
                "xn--bcher-kva.example",
                &["bücher.example", "BÜCHER.EXAMPLE."],
                false,
            ),
            ("localhost", &["localhost", "LOCALHOST", "localhost."], true),
            ("::1", &["::1", "0:0:0:0:0:0:0:1", "[::1]"], true),
        ];
        let ports = [1, DEFAULT_PORT, u16::MAX];
        let all_security = [
            TransportSecurity::VerifiedTls,
            TransportSecurity::InsecureTls,
            TransportSecurity::PlaintextLoopback,
        ];
        let mut generated = Vec::new();

        for (canonical, aliases, is_loopback) in alias_groups {
            for alias in aliases {
                for port in ports {
                    for security in all_security {
                        let result = Origin::from_uri(&uri_with_host(alias, port), security);
                        if security == TransportSecurity::PlaintextLoopback && !is_loopback {
                            assert!(result.is_err(), "plaintext unexpectedly accepted {alias}");
                            continue;
                        }
                        let origin = result.unwrap();
                        assert_eq!(origin.host(), canonical);
                        generated.push(((canonical, port, security), origin));
                    }
                }
            }
        }

        for (left_key, left) in &generated {
            for (right_key, right) in &generated {
                assert_eq!(left == right, left_key == right_key);
                assert_eq!(
                    left.storage_key().unwrap() == right.storage_key().unwrap(),
                    left_key == right_key
                );
            }
        }

        let distinct_reference_keys: HashSet<_> = generated.iter().map(|(key, _)| *key).collect();
        let distinct_storage_keys: HashSet<_> = generated
            .iter()
            .map(|(_, origin)| origin.storage_key().unwrap())
            .collect();
        assert_eq!(distinct_storage_keys.len(), distinct_reference_keys.len());
    }

    #[test]
    fn path_and_query_never_participate_in_origin_identity() {
        let paths = ["/", "/a", "/a/b/", "/other"];
        let queries = [None, Some("x=1"), Some("x=2&y=3")];
        let expected = Origin::from_uri(
            &uri_with_host("example.com", DEFAULT_PORT),
            TransportSecurity::VerifiedTls,
        )
        .unwrap();
        for path in paths {
            for query in queries {
                let uri = AtpUri::for_test("EXAMPLE.COM.", DEFAULT_PORT, path, query);
                assert_eq!(
                    Origin::from_uri(&uri, TransportSecurity::VerifiedTls).unwrap(),
                    expected
                );
            }
        }
    }
}
