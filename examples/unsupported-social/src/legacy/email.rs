use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};

/// SMTP configuration loaded from environment variables.
pub(crate) struct SmtpConfig {
    host: String,
    port: u16,
    user: String,
    pass: String,
    from: Mailbox,
}

impl SmtpConfig {
    /// Load SMTP config from env vars. Returns None if not configured.
    ///
    /// Required vars: `SMTP_HOST`, `SMTP_USER`, `SMTP_PASS`, `SMTP_FROM`
    /// Optional: `SMTP_PORT` (defaults to 587)
    pub fn from_env() -> Option<Self> {
        let host = std::env::var("SMTP_HOST").ok()?;
        let user = std::env::var("SMTP_USER").ok()?;
        let pass = std::env::var("SMTP_PASS").ok()?;
        let from_str = std::env::var("SMTP_FROM").ok()?;
        let from: Mailbox = from_str.parse().ok()?;
        let port = std::env::var("SMTP_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(587);

        Some(SmtpConfig {
            host,
            port,
            user,
            pass,
            from,
        })
    }

    /// Send a verification email with the given code.
    pub fn send_verification(
        &self,
        to_email: &str,
        to_name: &str,
        code: &str,
    ) -> Result<(), String> {
        let to: Mailbox = format!("{to_name} <{to_email}>")
            .parse()
            .map_err(|e| format!("Invalid email address: {e}"))?;

        let email = Message::builder()
            .from(self.from.clone())
            .to(to)
            .subject("Verify your DUSTNET account")
            .body(format!(
                "Hello {to_name},\n\n\
                 Your verification code is: {code}\n\n\
                 Enter this code at the verification page to complete your registration.\n\
                 This code expires in 1 hour.\n\n\
                 — DUSTNET"
            ))
            .map_err(|e| format!("Failed to build email: {e}"))?;

        let transport = SmtpTransport::starttls_relay(&self.host)
            .map_err(|e| format!("SMTP connection failed: {e}"))?
            .port(self.port)
            .credentials(Credentials::new(self.user.clone(), self.pass.clone()))
            .build();

        transport
            .send(&email)
            .map_err(|e| format!("Failed to send email: {e}"))?;

        Ok(())
    }
}

/// Check whether SMTP is configured (env vars are set).
pub(crate) fn smtp_configured() -> bool {
    std::env::var("SMTP_HOST").is_ok()
}
