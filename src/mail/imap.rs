use super::{message, wire::BoundedImapStream};
use crate::{
    App, net,
    store::mail::{self, MailContent, MailCredentials, PollMailbox},
};
use anyhow::{Context, Result, anyhow, ensure};
use async_imap::{Client, Session, types::Mailbox};
use base64::{Engine as _, engine::general_purpose};
use futures::{StreamExt, TryStreamExt, stream};
use rustls::{ClientConfig, RootCertStore, pki_types::ServerName};
use std::{
    fmt::Debug,
    sync::{Arc, LazyLock},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
    time::{self, MissedTickBehavior},
};
use tokio_rustls::{TlsConnector, client::TlsStream};

const POLL_INTERVAL: Duration = Duration::from_secs(15);
const MAILBOX_TIMEOUT: Duration = Duration::from_secs(30);
const UID_WINDOW: u32 = 1000;
const MAX_MESSAGES: usize = 10;
const MESSAGE_BYTES: usize = 256 * 1024;

static IMAP_TLS: LazyLock<TlsConnector> = LazyLock::new(|| {
    let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    TlsConnector::from(Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ))
});

pub async fn run(app: Arc<App>) {
    let mut ticker = time::interval(POLL_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let mailboxes = match mail::poll_mailboxes(&app.db, &app.vault).await {
            Ok(mailboxes) => mailboxes,
            Err(_) => {
                tracing::warn!("Could not read IMAP poll queue");
                continue;
            }
        };

        stream::iter(mailboxes)
            .for_each_concurrent(8, |mailbox| async {
                match time::timeout(MAILBOX_TIMEOUT, poll_mailbox(&app, mailbox)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) => tracing::debug!("Could not poll an IMAP folder"),
                    Err(_) => tracing::debug!("IMAP folder polling timed out"),
                }
            })
            .await;
    }
}

async fn connect(
    config: &MailCredentials,
) -> Result<Session<BoundedImapStream<TlsStream<TcpStream>>>> {
    ensure!(config.port == 993, "IMAP requires implicit TLS on port 993");
    let host = ServerName::try_from(config.host.clone())?;
    let addresses = net::resolve(&config.host, config.port).await?;
    let socket = TcpStream::connect(addresses.as_slice()).await?;
    let socket = IMAP_TLS.connect(host, socket).await?;
    let mut client = Client::new(BoundedImapStream::new(socket));
    client
        .read_response()
        .await?
        .context("Missing IMAP greeting")?;
    client
        .login(&config.username, &config.password)
        .await
        .map_err(|_| anyhow!("IMAP login rejected"))
}

async fn poll_mailbox(app: &App, mut mailbox: PollMailbox) -> Result<()> {
    let mut session = connect(&mailbox.imap).await?;
    let result = poll_messages(app, &mut mailbox, &mut session).await;
    let _ = time::timeout(Duration::from_secs(2), session.logout()).await;
    result
}

async fn poll_messages<T>(
    app: &App,
    mailbox: &mut PollMailbox,
    session: &mut Session<T>,
) -> Result<()>
where
    T: AsyncRead + AsyncWrite + Unpin + Debug + Send,
{
    let examined = session.examine(encode_mailbox(&mailbox.folder)).await?;
    let (uid_validity, last_available_uid) = cursor(&examined)?;
    if mailbox.uid_validity != Some(uid_validity) {
        mail::initialize_cursor(&app.db, mailbox, uid_validity, last_available_uid).await?;
        return Ok(());
    }
    if mailbox.last_uid >= last_available_uid {
        return Ok(());
    }

    let first_uid = mailbox.last_uid + 1;
    let last_checked_uid = first_uid
        .saturating_add(UID_WINDOW - 1)
        .min(last_available_uid);
    let mut uids = session
        .uid_search(format!("UID {first_uid}:{last_checked_uid}"))
        .await?
        .into_iter()
        .filter(|uid| (first_uid..=last_checked_uid).contains(uid))
        .collect::<Vec<_>>();
    uids.sort_unstable();
    let has_more = uids.len() > MAX_MESSAGES;
    for uid in uids.into_iter().take(MAX_MESSAGES) {
        let Some(content) = fetch_message(session, uid).await? else {
            // The message can be expunged by another client between SEARCH and FETCH.
            continue;
        };
        if !mail::record_delivery(&app.db, &app.vault, mailbox, uid_validity, uid, &content).await?
        {
            return Ok(());
        }
        mailbox.last_uid = uid;
    }
    if !has_more && last_checked_uid > mailbox.last_uid {
        mail::advance_cursor(&app.db, mailbox, uid_validity, last_checked_uid).await?;
    }
    Ok(())
}

fn cursor(mailbox: &Mailbox) -> Result<(u32, u32)> {
    let validity = mailbox
        .uid_validity
        .filter(|uid| *uid > 0)
        .context("Missing IMAP UIDVALIDITY")?;
    let next_uid = mailbox
        .uid_next
        .filter(|uid| *uid > 0)
        .context("Missing IMAP UIDNEXT")?;
    Ok((validity, next_uid - 1))
}

async fn fetch_message<T>(session: &mut Session<T>, uid: u32) -> Result<Option<MailContent>>
where
    T: AsyncRead + AsyncWrite + Unpin + Debug + Send,
{
    let mut response = session
        .uid_fetch(
            uid.to_string(),
            format!("(UID RFC822.SIZE BODY.PEEK[]<0.{MESSAGE_BYTES}>)"),
        )
        .await?;
    let mut content = None;
    let mut found = false;
    while let Some(fetch) = response.try_next().await? {
        if fetch.uid != Some(uid) {
            continue;
        }
        found = true;
        if let Some(bytes) = fetch.body() {
            let end = bytes.len().min(MESSAGE_BYTES);
            let partial = fetch.size.is_some_and(|size| size as usize > end) || end < bytes.len();
            content = Some(message::decode(&bytes[..end], partial));
        }
    }
    ensure!(!found || content.is_some(), "IMAP message body unavailable");
    Ok(content)
}

fn encode_mailbox(name: &str) -> String {
    // IMAP4rev1 represents non-ASCII folder names with modified UTF-7.
    let mut output = String::new();
    let mut utf16 = Vec::new();
    for character in name.chars() {
        if character.is_ascii() {
            push_encoded(&mut output, &mut utf16);
            if character == '&' {
                output.push_str("&-");
            } else {
                output.push(character);
            }
        } else {
            for code in character.encode_utf16(&mut [0; 2]) {
                utf16.extend_from_slice(&code.to_be_bytes());
            }
        }
    }
    push_encoded(&mut output, &mut utf16);
    output
}

fn push_encoded(output: &mut String, utf16: &mut Vec<u8>) {
    if !utf16.is_empty() {
        output.push('&');
        output.push_str(
            &general_purpose::STANDARD_NO_PAD
                .encode(&*utf16)
                .replace('/', ","),
        );
        output.push('-');
        utf16.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, duplex};

    #[test]
    fn imap_encodes_international_folder_names() {
        assert_eq!(encode_mailbox("INBOX"), "INBOX");
        assert_eq!(
            encode_mailbox("Sales & Support/日本語"),
            "Sales &- Support/&ZeVnLIqe-"
        );
    }

    #[tokio::test]
    async fn imap_examines_and_peeks_at_messages_without_marking_read() {
        let (client, server) = duplex(4096);
        let server = tokio::spawn(async move {
            let mut server = BufReader::new(server);
            server.write_all(b"* OK fixture ready\r\n").await.unwrap();
            let mut line = String::new();
            server.read_line(&mut line).await.unwrap();
            let (tag, command) = line.trim_end().split_once(' ').unwrap();
            assert_eq!(command, "LOGIN \"fixture\" \"synthetic-password\"");
            server
                .write_all(format!("{tag} OK LOGIN complete\r\n").as_bytes())
                .await
                .unwrap();
            line.clear();
            server.read_line(&mut line).await.unwrap();
            let (tag, command) = line.trim_end().split_once(' ').unwrap();
            assert_eq!(command, "EXAMINE \"INBOX\"");
            server
                .write_all(
                    format!("* 2 EXISTS\r\n* OK [UIDVALIDITY 97] UIDs valid\r\n* OK [UIDNEXT 3] Next UID\r\n{tag} OK [READ-ONLY] EXAMINE complete\r\n")
                        .as_bytes(),
                )
                .await
                .unwrap();
            line.clear();
            server.read_line(&mut line).await.unwrap();
            let (tag, command) = line.trim_end().split_once(' ').unwrap();
            assert_eq!(
                command,
                format!("UID FETCH 2 (UID RFC822.SIZE BODY.PEEK[]<0.{MESSAGE_BYTES}>)")
            );
            let message = "From: author@example.com\r\nSubject: Fixture\r\n\r\nA sample.";
            server
                .write_all(
                    format!(
                        "* 2 FETCH (UID 2 RFC822.SIZE {} BODY[]<0> {{{}}}\r\n{message})\r\n{tag} OK FETCH complete\r\n",
                        message.len(),
                        message.len()
                    ).as_bytes()
                ).await.unwrap();
        });
        time::timeout(Duration::from_secs(3), async {
            let mut client = Client::new(BoundedImapStream::new(client));
            client.read_response().await.unwrap().unwrap();
            let mut session = client.login("fixture", "synthetic-password").await.unwrap();
            let mailbox = session.examine("INBOX").await.unwrap();
            assert_eq!(cursor(&mailbox).unwrap(), (97, 2));
            let content = fetch_message(&mut session, 2).await.unwrap().unwrap();
            assert_eq!(content.subject, "Fixture");
            assert_eq!(content.text, "A sample.");
            server.await.unwrap();
        })
        .await
        .unwrap();
    }
}
