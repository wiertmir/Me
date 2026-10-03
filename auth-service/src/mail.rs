use std::sync::{Arc, Mutex};

use lettre::{
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor, message::Mailbox,
    transport::smtp::authentication::Credentials,
};

use crate::config::{SmtpConfig, SmtpTls};

pub struct Email {
    pub to: String,
    pub subject: String,
    pub body: String,
}

#[derive(Clone)]
pub enum Mailer {
    None,
    Smtp(Box<(AsyncSmtpTransport<Tokio1Executor>, Mailbox)>),
    /// Collects mail in memory (tests).
    Memory(Arc<Mutex<Vec<Email>>>),
}

impl Mailer {
    pub fn from_config(cfg: Option<&SmtpConfig>) -> anyhow::Result<Self> {
        let Some(c) = cfg else { return Ok(Self::None) };
        let builder = match c.tls {
            SmtpTls::Starttls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&c.host)?,
            SmtpTls::Implicit => AsyncSmtpTransport::<Tokio1Executor>::relay(&c.host)?,
            SmtpTls::None => {
                tracing::warn!(host = %c.host, "smtp tls = \"none\": mail is sent unencrypted; development use only");
                AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&c.host)
            }
        }
        .port(c.port);
        let builder = match (&c.username, &c.password) {
            (Some(u), Some(p)) => builder.credentials(Credentials::new(u.clone(), p.clone())),
            _ => builder,
        };
        Ok(Self::Smtp(Box::new((builder.build(), c.from.parse()?))))
    }

    pub fn enabled(&self) -> bool {
        !matches!(self, Self::None)
    }

    pub async fn send(&self, email: Email) -> anyhow::Result<()> {
        match self {
            Self::None => Ok(()),
            Self::Memory(m) => {
                m.lock().unwrap_or_else(|e| e.into_inner()).push(email);
                Ok(())
            }
            Self::Smtp(smtp) => {
                let (transport, from) = &**smtp;
                let msg = Message::builder()
                    .from(from.clone())
                    .to(Mailbox::new(None, email.to.parse::<lettre::Address>()?))
                    .subject(email.subject)
                    .body(email.body)?;
                transport.send(msg).await?;
                Ok(())
            }
        }
    }

    /// Sends without making the caller wait for the SMTP server, so response time does not
    /// reveal whether a mail was sent. Failures are logged (never the mail body).
    pub async fn dispatch(&self, email: Email) {
        let report = |r: anyhow::Result<()>| {
            if let Err(e) = r {
                tracing::error!(error = %e, "sending mail failed");
            }
        };
        if matches!(self, Self::Memory(_)) {
            report(self.send(email).await);
        } else {
            let this = self.clone();
            tokio::spawn(async move { report(this.send(email).await) });
        }
    }
}
