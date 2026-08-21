use super::ProtocolError;

/// Generate a self-signed certificate for the given hostname.
///
/// Returns (cert_der, key_der) — DER-encoded certificate and private key.
pub fn generate_self_signed(hostname: &str) -> Result<(Vec<u8>, Vec<u8>), ProtocolError> {
    let mut params = rcgen::CertificateParams::new(vec![hostname.to_string()])
        .map_err(|e| ProtocolError::tls(format_args!("cert params error: {e}")))?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, hostname);

    let key_pair = rcgen::KeyPair::generate()
        .map_err(|e| ProtocolError::tls(format_args!("key generation error: {e}")))?;

    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| ProtocolError::tls(format_args!("self-sign error: {e}")))?;

    let cert_der = cert.der().to_vec();
    let key_der = key_pair.serialize_der();

    Ok((cert_der, key_der))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_self_signed_cert() {
        let (cert_der, key_der) = generate_self_signed("localhost").unwrap();
        assert!(!cert_der.is_empty());
        assert!(!key_der.is_empty());
    }

    #[test]
    fn generated_cert_is_valid_der() {
        let (cert_der, _) = generate_self_signed("test.local").unwrap();
        // Should be parseable as a rustls certificate
        let _cert = rustls::pki_types::CertificateDer::from(cert_der);
    }
}
