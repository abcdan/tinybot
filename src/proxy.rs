mod upstream;

use crate::net;
use anyhow::{Context, Result, ensure};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional},
    net::{TcpListener, TcpStream},
    sync::Semaphore,
};
use upstream::Upstream;

pub async fn serve() -> Result<()> {
    let upstream = Arc::new(Upstream::configured()?);
    let listener = TcpListener::bind("0.0.0.0:3128").await?;
    let slots = Arc::new(Semaphore::new(96));
    loop {
        let (client, _) = listener.accept().await?;
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            continue;
        };
        let upstream = upstream.clone();
        tokio::spawn(async move {
            let _slot = slot;
            let _ = tokio::time::timeout(
                Duration::from_secs(50),
                handle(client, upstream.as_ref().as_ref()),
            )
            .await;
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

async fn handle(mut client: TcpStream, proxy: Option<&Upstream>) -> Result<()> {
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
    let addresses = async {
        let host = destination(std::str::from_utf8(&buffer[..header_end])?)?;
        net::resolve(&host, 443).await
    }
    .await;
    let Ok(addresses) = addresses else {
        client
            .write_all(b"HTTP/1.1 403 Forbidden\r\nConnection: close\r\nContent-Length: 0\r\n\r\n")
            .await?;
        return Ok(());
    };
    let upstream = tokio::time::timeout(Duration::from_secs(6), async {
        match proxy {
            Some(proxy) => proxy.connect(&addresses).await,
            None => Ok(TcpStream::connect(addresses.as_slice()).await?),
        }
    })
    .await;
    match upstream {
        Ok(Ok(mut upstream)) => {
            client
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await?;
            upstream.write_all(&buffer[header_end..]).await?;
            copy_bidirectional(&mut client, &mut upstream).await?;
        }
        _ => {
            client
                .write_all(
                    b"HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                )
                .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::task::JoinHandle;

    async fn relay(upstream: Upstream) -> (TcpStream, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            handle(socket, Some(&upstream)).await.unwrap();
        });
        (TcpStream::connect(address).await.unwrap(), server)
    }

    async fn http_headers(socket: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") {
            assert!(bytes.len() < 8192);
            bytes.push(socket.read_u8().await.unwrap());
        }
        String::from_utf8(bytes).unwrap()
    }

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

    #[test]
    fn proxy_configuration_accepts_only_supported_authorities() {
        assert!(Upstream::parse("http://proxy.example").is_ok());
        assert!(Upstream::parse("socks5://user:p%40ss@[::1]:1080").is_ok());
        for proxy in [
            "socks4://proxy.example:1080",
            "socks5://proxy.example",
            "http://proxy.example:0",
            "http://user%3Aname:pw@proxy.example:3128",
            "socks5://user@proxy.example:1080",
            "http://proxy.example:3128/path?secret=fixture",
            "http://user:p%0D%0Ass@proxy.example:3128",
        ] {
            assert!(Upstream::parse(proxy).is_err());
        }
    }

    #[tokio::test]
    async fn http_upstream_receives_ip_connect_and_only_its_own_credentials() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream = Upstream::parse(&format!(
            "http://relay:p%40ss@{}",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let proxy = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let headers = http_headers(&mut socket).await;
            assert!(headers.starts_with("CONNECT 1.1.1.1:443 HTTP/1.1\r\nHost: 1.1.1.1:443\r\n"));
            assert!(headers.contains("Proxy-Authorization: Basic cmVsYXk6cEBzcw==\r\n"));
            assert!(!headers.contains("browser-fixture"));
            socket
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .unwrap();
            let mut bytes = [0; 4];
            socket.read_exact(&mut bytes).await.unwrap();
            assert_eq!(&bytes, b"ping");
            socket.write_all(b"pong").await.unwrap();
        });
        let (mut client, server) = relay(upstream).await;
        client
            .write_all(
                b"CONNECT 1.1.1.1:443 HTTP/1.1\r\nProxy-Authorization: browser-fixture\r\n\r\nping",
            )
            .await
            .unwrap();
        assert!(http_headers(&mut client).await.starts_with("HTTP/1.1 200 "));
        let mut reply = Vec::new();
        client.read_to_end(&mut reply).await.unwrap();
        assert_eq!(&reply, b"pong");
        client.shutdown().await.unwrap();
        proxy.await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn socks5_upstream_authenticates_and_uses_a_numeric_ipv6_target() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream = Upstream::parse(&format!(
            "socks5://relay:p%40ss@{}",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let proxy = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut negotiation = [0; 3];
            socket.read_exact(&mut negotiation).await.unwrap();
            assert_eq!(negotiation, [5, 1, 2]);
            socket.write_all(&[5, 2]).await.unwrap();

            let mut auth = [0; 12];
            socket.read_exact(&mut auth).await.unwrap();
            assert_eq!(&auth, b"\x01\x05relay\x04p@ss");
            socket.write_all(&[1, 0]).await.unwrap();

            let mut request = [0; 22];
            socket.read_exact(&mut request).await.unwrap();
            let target: std::net::Ipv6Addr = "2606:4700:4700::1111".parse().unwrap();
            assert_eq!(&request[..4], &[5, 1, 0, 4]);
            assert_eq!(&request[4..20], &target.octets());
            assert_eq!(&request[20..], &443_u16.to_be_bytes());
            socket
                .write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0])
                .await
                .unwrap();
            assert_eq!(socket.read_u8().await.unwrap(), 7);
            socket.write_u8(8).await.unwrap();
        });
        let (mut client, server) = relay(upstream).await;
        client
            .write_all(b"CONNECT [2606:4700:4700::1111]:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        assert!(http_headers(&mut client).await.starts_with("HTTP/1.1 200 "));
        client.write_u8(7).await.unwrap();
        assert_eq!(client.read_u8().await.unwrap(), 8);
        client.shutdown().await.unwrap();
        proxy.await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn proxy_rejection_does_not_fall_back_to_direct_access() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream =
            Upstream::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let proxy = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            assert!(
                !http_headers(&mut socket)
                    .await
                    .contains("Proxy-Authorization")
            );
            socket
                .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n")
                .await
                .unwrap();
        });
        let (mut client, server) = relay(upstream).await;
        client
            .write_all(b"CONNECT 1.1.1.1:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        assert!(http_headers(&mut client).await.starts_with("HTTP/1.1 502 "));
        proxy.await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_private_target_cannot_reach_the_configured_proxy() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream =
            Upstream::parse(&format!("socks5://{}", listener.local_addr().unwrap())).unwrap();
        let (mut client, server) = relay(upstream).await;
        client
            .write_all(b"CONNECT 127.0.0.1:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        assert!(http_headers(&mut client).await.starts_with("HTTP/1.1 403 "));
        server.await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn private_target_is_rejected_before_connecting() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            handle(listener.accept().await.unwrap().0, None)
                .await
                .unwrap()
        });
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
