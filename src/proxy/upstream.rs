use anyhow::{Context, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use http::{HeaderValue, Uri};
use hyper_util::{
    client::legacy::connect::{
        HttpConnector,
        proxy::{SocksV5, Tunnel},
    },
    rt::TokioIo,
};
use percent_encoding::percent_decode_str;
use std::{future::poll_fn, net::SocketAddr};
use tokio::net::TcpStream;
use tower_service::Service;
use url::Url;

#[derive(Clone)]
pub(super) enum Upstream {
    Http(Tunnel<HttpConnector>),
    Socks5(SocksV5<HttpConnector>),
}

impl Upstream {
    pub(super) fn configured() -> Result<Option<Self>> {
        match std::env::var("TINYBOT_BROWSER_PROXY") {
            Ok(value) if !value.trim().is_empty() => Self::parse(value.trim())
                .map(Some)
                .context("invalid TINYBOT_BROWSER_PROXY"),
            Ok(_) | Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => {
                bail!("invalid TINYBOT_BROWSER_PROXY encoding")
            }
        }
    }

    pub(super) fn parse(raw: &str) -> Result<Self> {
        let url = Url::parse(raw).context("expected a proxy URL")?;
        ensure!(
            matches!(url.scheme(), "http" | "socks5"),
            "expected an http:// or socks5:// proxy"
        );
        ensure!(
            matches!(url.path(), "" | "/") && url.query().is_none() && url.fragment().is_none(),
            "the proxy URL must contain an authority without a path, query, or fragment"
        );
        let host = url.host_str().context("missing proxy hostname")?;
        let port = url
            .port_or_known_default()
            .filter(|port| *port != 0)
            .context("missing or invalid proxy port")?;

        // The connector only receives the proxy socket endpoint. Its userinfo is handled
        // separately so credentials never appear in connector URLs or diagnostics.
        let server: Uri = format!("http://{host}:{port}/")
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid proxy authority"))?;
        let username = userinfo(url.username())?;
        let password = userinfo(url.password().unwrap_or_default())?;
        let authenticated = !username.is_empty() || !password.is_empty();
        ensure!(
            !authenticated || !username.is_empty(),
            "proxy authentication requires a username"
        );

        let connector = HttpConnector::new();
        if url.scheme() == "http" {
            let mut tunnel = Tunnel::new(server, connector);
            if authenticated {
                ensure!(!username.contains(':'), "invalid HTTP proxy username");
                let mut auth = HeaderValue::from_str(&format!(
                    "Basic {}",
                    STANDARD.encode(format!("{username}:{password}"))
                ))
                .context("invalid HTTP proxy credentials")?;
                auth.set_sensitive(true);
                tunnel = tunnel.with_auth(auth);
            }
            return Ok(Self::Http(tunnel));
        }
        let mut socks = SocksV5::new(server, connector);
        if authenticated {
            ensure!(
                username.len() <= 255 && (1..=255).contains(&password.len()),
                "invalid SOCKS5 proxy credentials"
            );
            socks = socks.with_auth(username, password);
        }
        Ok(Self::Socks5(socks))
    }

    pub(super) async fn connect(&self, targets: &[SocketAddr]) -> Result<TcpStream> {
        let mut upstream = self.clone();
        for target in targets {
            // The browser still uses the original URL and verifies origin TLS. The proxy
            // receives only the IP we already resolved and validated locally.
            let target = format!("https://{target}/")
                .parse()
                .context("invalid tunnel target")?;
            let connection = match &mut upstream {
                Self::Http(connector) => connect(connector, target).await,
                Self::Socks5(connector) => connect(connector, target).await,
            };
            if let Some(connection) = connection {
                return Ok(connection);
            }
        }
        bail!("upstream proxy did not open a tunnel")
    }
}

fn userinfo(value: &str) -> Result<String> {
    let value = percent_decode_str(value)
        .decode_utf8()
        .context("invalid proxy credentials")?;
    ensure!(
        !value.chars().any(char::is_control),
        "invalid proxy credentials"
    );
    Ok(value.into_owned())
}

async fn connect<C>(connector: &mut C, target: Uri) -> Option<TcpStream>
where
    C: Service<Uri, Response = TokioIo<TcpStream>>,
{
    poll_fn(|cx| connector.poll_ready(cx)).await.ok()?;
    connector.call(target).await.ok().map(TokioIo::into_inner)
}
