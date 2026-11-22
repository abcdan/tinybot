use anyhow::{Context, Result, ensure};
use ipnet::IpNet;
use reqwest::{Client, RequestBuilder};
use std::{
    net::{IpAddr, SocketAddr},
    sync::LazyLock,
    time::Duration,
};
use url::Url;

static SPECIAL: LazyLock<Vec<IpNet>> = LazyLock::new(|| {
    [
        "0.0.0.0/8",
        "10.0.0.0/8",
        "100.64.0.0/10",
        "127.0.0.0/8",
        "169.254.0.0/16",
        "172.16.0.0/12",
        "192.0.0.0/24",
        "192.0.2.0/24",
        "192.88.99.0/24",
        "192.168.0.0/16",
        "198.18.0.0/15",
        "198.51.100.0/24",
        "203.0.113.0/24",
        "224.0.0.0/3",
        "2001::/23",
        "2001:db8::/32",
        "2002::/16",
        "3fff::/20",
    ]
    .into_iter()
    .map(|range| range.parse().unwrap())
    .collect()
});

pub fn public(ip: IpAddr) -> bool {
    let ip = ip.to_canonical();
    if let IpAddr::V6(v6) = ip {
        if v6.segments()[0] & 0xe000 != 0x2000 {
            return false;
        }
    }
    !SPECIAL.iter().any(|range| range.contains(&ip))
}

pub async fn resolve(host: &str, port: u16) -> Result<Vec<SocketAddr>> {
    let addresses: Vec<_> = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::lookup_host((host, port)),
    )
    .await??
    .take(32)
    .collect();
    ensure!(
        !addresses.is_empty() && addresses.iter().all(|address| public(address.ip())),
        "destination is not public"
    );
    Ok(addresses)
}

pub fn https(raw: &str) -> Result<Url> {
    let url = Url::parse(raw)?;
    ensure!(
        url.scheme() == "https"
            && url.port_or_known_default() == Some(443)
            && url.host().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none(),
        "expected a public HTTPS URL on port 443"
    );
    Ok(url)
}

pub async fn request(method: reqwest::Method, url: &Url) -> Result<RequestBuilder> {
    https(url.as_str())?;
    let host = url
        .host_str()
        .context("missing host")?
        .trim_matches(['[', ']']);
    let addresses = resolve(host, 443).await?;
    let client = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .resolve_to_addrs(host, &addresses)
        .build()?;
    Ok(client.request(method, url.clone()))
}

pub async fn body(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            output.len() + chunk.len() <= limit,
            "response exceeds size limit"
        );
        output.extend_from_slice(&chunk);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_internal_and_special_addresses() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "169.254.169.254",
            "::1",
            "::ffff:127.0.0.1",
            "64:ff9b::a00:1",
            "100.64.0.1",
            "2002:7f00:1::1",
        ] {
            assert!(!public(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["1.1.1.1", "2606:4700:4700::1111"] {
            assert!(public(ip.parse().unwrap()), "{ip}");
        }
    }
}
