//! Minimal transactional email sender (password reset, invitation).
//!
//! Wraps `lettre` SMTP transport. When SMTP is not configured (`AppConfig.email`
//! is `None`), the [`Mailer`] is `None` in [`AppState`] and password-reset
//! requests remain enumeration-safe no-ops. Recovery credentials are never
//! written to logs.
//!
//! ## Design decisions
//! - **No queue.** Emails are sent synchronously in the request handler. For a
//!   low-volume transactional path (password reset) this is acceptable — the
//!   SMTP round-trip adds ~100–500ms but saves an entire job-queue subsystem.
//! - **Plain-text only.** No HTML template engine; the reset-link body is a
//!   simple format string. A later sprint can add a `tera`/`handlebars` renderer.
//! - **Single global config.** Per-workspace SMTP settings are a future concern
//!   (RFE if enterprise customers demand branded "From" addresses).

use lettre::{
    message::Mailbox, transport::smtp::authentication::Credentials, AsyncSmtpTransport,
    AsyncTransport, Message, Tokio1Executor,
};
use tracing::warn;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SmtpSecurity {
    PlaintextLoopback,
    StartTls,
    ImplicitTls,
}

fn is_loopback_host(host: &str) -> bool {
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn smtp_security(cfg: &aero_common::config::EmailConfig) -> Result<SmtpSecurity, &'static str> {
    if cfg.allow_insecure_localhost {
        if is_loopback_host(&cfg.host) {
            return Ok(SmtpSecurity::PlaintextLoopback);
        }
        return Err("plaintext SMTP is restricted to localhost or loopback IPs");
    }
    if cfg.starttls {
        Ok(SmtpSecurity::StartTls)
    } else {
        Ok(SmtpSecurity::ImplicitTls)
    }
}

/// Wires up an optional SMTP transport from config.
/// Returns `None` when no email config is present.
pub fn build_mailer(cfg: Option<&aero_common::config::EmailConfig>) -> Option<Mailer> {
    let ec = cfg?;
    let from: Mailbox = ec
        .from
        .parse()
        .map_err(|e| {
            warn!(from = %ec.from, error = ?e, "invalid email.from address");
            e
        })
        .ok()?;

    let security = smtp_security(ec)
        .map_err(|error| {
            warn!(host = %ec.host, error, "refusing unsafe SMTP configuration");
            error
        })
        .ok()?;
    let builder = match security {
        SmtpSecurity::PlaintextLoopback => {
            AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&ec.host)
        }
        SmtpSecurity::StartTls => {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&ec.host)
                .map_err(|error| {
                    warn!(host = %ec.host, error = ?error, "failed to create STARTTLS SMTP transport");
                    error
                })
                .ok()?
        }
        SmtpSecurity::ImplicitTls => AsyncSmtpTransport::<Tokio1Executor>::relay(&ec.host)
            .map_err(|error| {
                warn!(host = %ec.host, error = ?error, "failed to create implicit-TLS SMTP transport");
                error
            })
            .ok()?,
    };
    let mut builder = builder.port(ec.port);
    match (ec.username.is_empty(), ec.password.is_empty()) {
        (true, true) => {}
        (false, false) => {
            builder =
                builder.credentials(Credentials::new(ec.username.clone(), ec.password.clone()));
        }
        _ => {
            warn!(
                host = %ec.host,
                "SMTP username and password must either both be set or both be empty"
            );
            return None;
        }
    }
    let transport = builder.build();

    Some(Mailer { transport, from })
}

/// Handle for sending transactional emails.
#[derive(Clone)]
pub struct Mailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
}

impl Mailer {
    /// Send a password-reset email.
    ///
    /// Constructs a plain-text email with the reset link. Errors are logged and
    /// swallowed — the caller MUST NOT fail the HTTP request if email delivery
    /// fails (still return 200 to avoid email enumeration).
    pub async fn send_password_reset(&self, to_addr: &str, token: &str) {
        let to: Mailbox = match to_addr.parse() {
            Ok(m) => m,
            Err(e) => {
                warn!(to = to_addr, error = ?e, "invalid recipient address for password reset");
                return;
            }
        };
        let body = format!(
            "Your password reset link:\n\n  {token}\n\n\
             This token expires in one hour and is single-use.\n\
             If you did not request this reset, ignore this email.\n"
        );
        let email = match Message::builder()
            .from(self.from.clone())
            .to(to)
            .subject("Password Reset — Aero IM")
            .body(body)
        {
            Ok(m) => m,
            Err(e) => {
                warn!(error = ?e, "failed to build reset email message");
                return;
            }
        };
        match self.transport.send(email).await {
            Ok(r) => {
                tracing::info!(to = to_addr, response = ?r.message().collect::<Vec<_>>(), "password reset email sent")
            }
            Err(e) => warn!(to = to_addr, error = ?e, "failed to send password reset email"),
        }
    }

    /// Send a workspace invitation email.
    ///
    /// Errors are logged and swallowed — the API response already returned the
    /// invite URL to the admin, so this is additive. The admin can share the
    /// URL manually if SMTP is unavailable.
    pub async fn send_invitation(&self, to_addr: &str, invite_url: &str, workspace_name: &str) {
        let to: Mailbox = match to_addr.parse() {
            Ok(m) => m,
            Err(e) => {
                warn!(to = to_addr, error = ?e, "invalid recipient address for invitation");
                return;
            }
        };
        let body = format!(
            "You have been invited to join {workspace_name} on Aero IM.\n\n\
             Click the link below to accept the invitation:\n\n  {invite_url}\n\n\
             This invitation link expires and is single-use.\n"
        );
        let email = match Message::builder()
            .from(self.from.clone())
            .to(to)
            .subject("You're invited to join Aero IM")
            .body(body)
        {
            Ok(m) => m,
            Err(e) => {
                warn!(error = ?e, "failed to build invitation email message");
                return;
            }
        };
        match self.transport.send(email).await {
            Ok(_) => tracing::info!(to = to_addr, "invitation email sent"),
            Err(e) => warn!(to = to_addr, error = ?e, "failed to send invitation email"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aero_common::config::EmailConfig;

    fn email_config(host: &str, starttls: bool, allow_insecure_localhost: bool) -> EmailConfig {
        EmailConfig {
            host: host.into(),
            port: 1025,
            username: String::new(),
            password: String::new(),
            from: "noreply@example.test".into(),
            starttls,
            allow_insecure_localhost,
        }
    }

    #[test]
    fn plaintext_smtp_requires_explicit_loopback_configuration() {
        let local = email_config("127.0.0.1", true, true);
        assert_eq!(smtp_security(&local), Ok(SmtpSecurity::PlaintextLoopback));
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("::1"));
        assert!(is_loopback_host("[::1]"));

        let remote = email_config("smtp.example.test", true, true);
        assert!(smtp_security(&remote).is_err());
        assert!(!is_loopback_host("192.0.2.10"));
    }

    #[test]
    fn secure_smtp_mode_honors_starttls_flag() {
        let starttls = email_config("smtp.example.test", true, false);
        assert_eq!(smtp_security(&starttls), Ok(SmtpSecurity::StartTls));

        let implicit_tls = email_config("smtp.example.test", false, false);
        assert_eq!(smtp_security(&implicit_tls), Ok(SmtpSecurity::ImplicitTls));
    }

    #[test]
    fn mailer_rejects_partial_auth_configuration() {
        let mut config = email_config("127.0.0.1", false, true);
        config.username = "mailer".into();
        assert!(build_mailer(Some(&config)).is_none());
    }
}
