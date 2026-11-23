use crate::{net, store::mail::SmtpConnection};
use anyhow::{Result, anyhow, ensure};
use lettre::{
    Address, Message,
    message::{Mailbox, header::ContentType},
    transport::smtp::{
        authentication::{Credentials, Mechanism},
        client::{AsyncSmtpConnection, TlsParameters},
        extension::ClientId,
    },
};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::time::timeout;
use uuid::Uuid;

const CONNECTION_TIMEOUT: Duration = Duration::from_secs(15);
const SEND_TIMEOUT: Duration = Duration::from_secs(25);

#[derive(Serialize, Deserialize)]
pub struct SmtpSend {
    pub to: Vec<String>,
    pub subject: String,
    pub text: String,
    pub in_reply_to: Option<String>,
}

impl SmtpSend {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=5).contains(&self.to.len()),
            "Provide one to five email recipients."
        );
        for recipient in &self.to {
            address(recipient)?;
        }
        ensure!(
            self.subject.len() <= 180 && !self.subject.chars().any(char::is_control),
            "Email subject must be at most 180 bytes and contain no control characters."
        );
        ensure!(
            !self.text.trim().is_empty()
                && self.text.len() <= 2400
                && !self
                    .text
                    .chars()
                    .any(|c| c.is_control() && !matches!(c, '\r' | '\n' | '\t')),
            "Provide email text of at most 2400 bytes."
        );
        ensure!(
            self.in_reply_to.as_deref().is_none_or(valid_message_id),
            "Invalid email reply message ID."
        );
        Ok(())
    }

    fn message(&self, sender: &str, pending_id: &str) -> Result<Message> {
        self.validate()?;
        let id = Uuid::parse_str(pending_id).map_err(|_| anyhow!("Invalid mail approval ID."))?;
        let mut builder = Message::builder()
            .from(Mailbox::new(None, address(sender)?))
            .subject(&self.subject)
            .message_id(Some(format!("<{id}@tinybot.invalid>")));
        for recipient in &self.to {
            builder = builder.to(Mailbox::new(None, address(recipient)?));
        }
        if let Some(reply) = &self.in_reply_to {
            builder = builder.in_reply_to(reply.clone()).references(reply.clone());
        }
        Ok(builder
            .header(ContentType::TEXT_PLAIN)
            .body(self.text.clone())?)
    }
}

pub(super) fn address(value: &str) -> Result<Address> {
    ensure!(
        value.len() <= 254
            && !value
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '<' | '>')),
        "Provide an email address without a display name."
    );
    value.parse().map_err(|_| anyhow!("Invalid email address."))
}

pub(super) fn valid_message_id(value: &str) -> bool {
    value.len() <= 256
        && value
            .strip_prefix('<')
            .and_then(|value| value.strip_suffix('>'))
            .is_some_and(|inner| inner.is_ascii() && address(inner).is_ok())
}

pub async fn send(connection: &SmtpConnection, draft: &SmtpSend, pending_id: &str) -> Result<()> {
    let message = draft.message(&connection.address, pending_id)?;
    let endpoint = &connection.credentials;
    ensure!(matches!(endpoint.port, 465 | 587), "Invalid SMTP port.");
    let tls = TlsParameters::new(endpoint.host.clone())
        .map_err(|_| anyhow!("Unable to configure SMTP TLS."))?;
    let hello = ClientId::default();
    let addresses = net::resolve(&endpoint.host, endpoint.port)
        .await
        .map_err(|_| anyhow!("Unable to resolve the SMTP server to a public address."))?;
    let mut client = timeout(CONNECTION_TIMEOUT, async {
        let mut client = match endpoint.port {
            465 => {
                AsyncSmtpConnection::connect_tokio1(
                    addresses.as_slice(),
                    Some(CONNECTION_TIMEOUT),
                    &hello,
                    Some(tls),
                    None,
                )
                .await?
            }
            _ => starttls(addresses.as_slice(), tls, &hello).await?,
        };
        ensure!(client.is_encrypted(), "SMTP TLS is required.");
        client
            .auth(
                &[Mechanism::Plain, Mechanism::Login],
                &Credentials::new(endpoint.username.clone(), endpoint.password.clone()),
            )
            .await?;
        Ok::<_, anyhow::Error>(client)
    })
    .await
    .map_err(|_| anyhow!("Connecting to the SMTP server timed out."))?
    .map_err(|_| {
        anyhow!("Could not establish an authenticated TLS session with the SMTP server.")
    })?;

    timeout(
        SEND_TIMEOUT,
        client.send(message.envelope(), &message.formatted()),
    )
    .await
    .map_err(|_| anyhow!("SMTP timed out. Delivery could not be confirmed."))?
    .map_err(|_| anyhow!("The SMTP server did not confirm delivery."))?;
    let _ = timeout(Duration::from_secs(2), client.quit()).await;
    Ok(())
}

async fn starttls(
    addresses: &[std::net::SocketAddr],
    tls: TlsParameters,
    hello: &ClientId,
) -> Result<AsyncSmtpConnection> {
    let mut client =
        AsyncSmtpConnection::connect_tokio1(addresses, Some(CONNECTION_TIMEOUT), hello, None, None)
            .await?;
    ensure!(
        client.can_starttls(),
        "SMTP server must advertise STARTTLS."
    );
    client.starttls(tls, hello).await?;
    Ok(client)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
    };

    #[test]
    fn smtp_message_matches_approved_address_and_headers() {
        let mut draft = SmtpSend {
            to: vec!["reader@example.com".into(), "editor@example.com".into()],
            subject: "An update".into(),
            text: "Hello,\r\n.notes".into(),
            in_reply_to: Some("<incoming@example.com>".into()),
        };
        let pending_id = "aec0113a-fdc5-4a00-83aa-bd29a2fcbdea";
        let message = draft.message("sender@example.com", pending_id).unwrap();
        let formatted = String::from_utf8(message.formatted()).unwrap();
        assert_eq!(message.envelope().to().len(), 2);
        assert!(formatted.contains(&format!("Message-ID: <{pending_id}@tinybot.invalid>")));
        assert!(formatted.contains("In-Reply-To: <incoming@example.com>"));
        assert!(formatted.contains("References: <incoming@example.com>"));
        assert!(formatted.contains("\r\n\r\nHello,\r\n.notes"));
        draft.to[0] = "Bob <reader@example.com>".into();
        assert!(draft.validate().is_err());
        draft.to[0] = "reader@example.com".into();
        draft.subject = "Report\r\nBcc: other@example.com".into();
        assert!(draft.validate().is_err());
        draft.subject = "Report".into();
        draft.in_reply_to = Some("<one@example.com> <two@example.com>".into());
        assert!(draft.validate().is_err());
    }

    #[tokio::test]
    async fn smtp_starttls_rejects_a_server_advertising_only_cleartext_auth() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(socket);
            socket
                .write_all(b"220 mail.example.com ready\r\n")
                .await
                .unwrap();
            let mut command = String::new();
            socket.read_line(&mut command).await.unwrap();
            assert!(command.starts_with("EHLO "));
            socket
                .write_all(b"250-mail.example.com\r\n250 AUTH PLAIN LOGIN\r\n")
                .await
                .unwrap();
            command.clear();
            if socket.read_line(&mut command).await.unwrap_or_default() > 0 {
                assert_eq!(command, "QUIT\r\n");
            }
        });
        let tls = TlsParameters::new("mail.example.com".into()).unwrap();
        let hello = ClientId::default();
        let result = timeout(Duration::from_secs(2), starttls(&[addr], tls, &hello)).await;
        assert!(result.unwrap().is_err());
        timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
    }
}
