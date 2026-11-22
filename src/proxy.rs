use crate::net;
use anyhow::{Context, Result, ensure};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional},
    net::{TcpListener, TcpStream},
    sync::Semaphore,
};

pub async fn serve() -> Result<()> {
    let listener = TcpListener::bind("0.0.0.0:3128").await?;
    let slots = Arc::new(Semaphore::new(96));
    loop {
        let (client, _) = listener.accept().await?;
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            continue;
        };
        tokio::spawn(async move {
            let _slot = slot;
            let _ = tokio::time::timeout(Duration::from_secs(50), handle(client)).await;
        });
    }
}

fn destination(header: &str) -> Result<String> {
    let line = header
        .split("\r\n")
        .next()
        .context("missing request line")?;
    let mut parts = line.split_ascii_whitespace();
    ensure!(
        parts.next() == Some("CONNECT"),
        "only HTTPS CONNECT is available"
    );
    let authority = parts.next().context("missing CONNECT authority")?;
    ensure!(
        matches!(parts.next(), Some("HTTP/1.1" | "HTTP/1.0"))
            && parts.next().is_none()
            && authority.ends_with(":443")
            && !authority.contains(['/', '\\', '?', '#', '@']),
        "invalid CONNECT authority"
    );
    let url = net::https(&format!("https://{authority}"))?;
    Ok(url
        .host_str()
        .context("missing target host")?
        .trim_matches(['[', ']'])
        .into())
}

async fn handle(mut client: TcpStream) -> Result<()> {
    let mut buffer = Vec::with_capacity(2048);
    let mut chunk = [0; 1024];
    let header_end = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let count = client.read(&mut chunk).await?;
            ensure!(count != 0, "proxy client disconnected");
            buffer.extend_from_slice(&chunk[..count]);
            if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                return Ok::<_, anyhow::Error>(end + 4);
            }
            ensure!(buffer.len() < 8192, "proxy request exceeds size limit");
        }
    })
    .await??;
    let upstream = async {
        let host = destination(std::str::from_utf8(&buffer[..header_end])?)?;
        let addresses = net::resolve(&host, 443).await?;
        Ok::<_, anyhow::Error>(
            tokio::time::timeout(
                Duration::from_secs(6),
                TcpStream::connect(addresses.as_slice()),
            )
            .await??,
        )
    }
    .await;
    match upstream {
        Ok(mut upstream) => {
            client
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await?;
            upstream.write_all(&buffer[header_end..]).await?;
            copy_bidirectional(&mut client, &mut upstream).await?;
        }
        Err(_) => {
            client
                .write_all(
                    b"HTTP/1.1 403 Forbidden\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                )
                .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_accepts_only_https_authorities() {
        assert_eq!(
            destination("CONNECT example.org:443 HTTP/1.1\r\n\r\n").unwrap(),
            "example.org"
        );
        for request in [
            "GET https://example.org HTTP/1.1",
            "CONNECT example.org:80 HTTP/1.1",
            "CONNECT user@example.org:443 HTTP/1.1",
            "CONNECT example.org/path:443 HTTP/1.1",
        ] {
            assert!(destination(request).is_err());
        }
    }

    #[tokio::test]
    async fn private_target_is_rejected_before_connecting() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server =
            tokio::spawn(async move { handle(listener.accept().await.unwrap().0).await.unwrap() });
        let mut client = TcpStream::connect(address).await.unwrap();
        client
            .write_all(b"CONNECT 169.254.169.254:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 403"));
        server.await.unwrap();
    }
}
