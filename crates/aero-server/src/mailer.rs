//! Minimal transactional email sender (password reset, invitation).
//!
//! Wraps `lettre` SMTP transport. When SMTP is not configured (`AppConfig.email`
//! is `None`), the [`Mailer`] is `None` in [`AppState`] and callers fall back to
//! logging the token — suitable for development.
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
    message::Mailbox,
    transport::smtp::authentication::Credentials,
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
};
use tracing::warn;

/// Wires up an optional SMTP transport from config.
/// Returns `None` when no email config is present (development / log-only mode).
pub fn build_mailer(cfg: Option<&aero_common::config::EmailConfig>) -> Option<Mailer> {
    let ec = cfg?;
    let creds = Credentials::new(ec.username.clone(), ec.password.clone());
    let from: Mailbox = ec.from.parse().map_err(|e| {
        warn!(from = %ec.from, error = ?e, "invalid email.from address");
        e
    }).ok()?;

    let transport = AsyncSmtpTransport::<Tokio1Executor>::relay(&ec.host)
        .map_err(|e| {
            warn!(host = %ec.host, error = ?e, "failed to create SMTP transport");
            e
        })
        .ok()?
        .port(ec.port)
        .credentials(creds)
        .build();

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
            Ok(r) => tracing::info!(to = to_addr, response = ?r.message().collect::<Vec<_>>(), "password reset email sent"),
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
