use crate::net;
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
use url::{Host, Url};

pub(super) struct ProxyUrl {
    protocol: Protocol,
    server: Uri,
    auth: Option<(String, String)>,
}

#[derive(Clone, Copy)]
enum Protocol {
    Http,
    Socks5,
}

impl Protocol {
    fn scheme(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Socks5 => "socks5",
        }
    }
}

impl ProxyUrl {
    fn parse(raw: &str) -> Result<Self> {
        ensure!(!raw.chars().any(char::is_control), "invalid proxy URL");
        let url = Url::parse(raw).context("expected a proxy URL")?;
        let protocol = match url.scheme() {
            "http" => Protocol::Http,
            "socks5" => Protocol::Socks5,
            _ => bail!("expected an http:// or socks5:// proxy"),
        };
        ensure!(
            matches!(url.path(), "" | "/") && url.query().is_none() && url.fragment().is_none(),
            "the proxy URL must contain an authority without a path, query, or fragment"
        );
        let host = url.host_str().context("missing proxy hostname")?;
        let port = url
            .port_or_known_default()
            .filter(|port| *port != 0)
            .context("missing or invalid proxy port")?;

        // Keep userinfo separate from the connector's socket endpoint.
        let server: Uri = format!("http://{host}:{port}/")
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid proxy authority"))?;
        let username = userinfo(url.username())?;
        let password = userinfo(url.password().unwrap_or_default())?;
        let auth = if username.is_empty() && password.is_empty() {
            None
        } else {
            ensure!(
                !username.is_empty(),
                "proxy authentication requires a username"
            );
            match protocol {
                Protocol::Http => ensure!(!username.contains(':'), "invalid HTTP proxy username"),
                Protocol::Socks5 => ensure!(
                    username.len() <= 255 && (1..=255).contains(&password.len()),
                    "invalid SOCKS5 proxy credentials"
                ),
            }
            Some((username, password))
        };
        Ok(Self {
            protocol,
            server,
            auth,
        })
    }

    pub(super) fn public(raw: &str) -> Result<Self> {
        let proxy = Self::parse(raw)?;
        let host = Host::parse(proxy.server.host().context("missing proxy hostname")?)
            .context("invalid proxy hostname")?;
        let public = match host {
            Host::Ipv4(ip) => net::public(ip.into()),
            Host::Ipv6(ip) => net::public(ip.into()),
            Host::Domain(host) => {
                let host = host.trim_end_matches('.');
                host.contains('.') && !host.ends_with(".localhost") && !host.ends_with(".local")
            }
        };
        ensure!(public, "proxy endpoint must be public");
        Ok(proxy)
    }

    pub(super) fn endpoint(&self) -> String {
        format!(
            "{}://{}",
            self.protocol.scheme(),
            self.server
                .authority()
                .expect("proxy URLs have an authority")
        )
    }

    pub(super) fn authenticated(&self) -> bool {
        self.auth.is_some()
    }

    fn host(&self) -> &str {
        self.server
            .host()
            .expect("proxy URLs have a host")
            .trim_matches(['[', ']'])
    }

    pub(super) async fn connect(&self, targets: &[SocketAddr]) -> Result<TcpStream> {
        let port = self.server.port_u16().context("missing proxy port")?;
        let relays = net::resolve(self.host(), port).await?;
        for relay in relays {
            // Pin the relay as well as the website. The connector must not
            // resolve a second, potentially different address for the relay.
            let server = format!("http://{relay}/")
                .parse()
                .context("invalid proxy authority")?;
            if let Ok(tunnel) = Upstream::new(self, server)?.connect(targets).await {
                return Ok(tunnel);
            }
        }
        bail!("upstream proxy did not open a tunnel")
    }
}

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
        let url = ProxyUrl::parse(raw)?;
        Self::new(&url, url.server.clone())
    }

    fn new(url: &ProxyUrl, server: Uri) -> Result<Self> {
        let connector = HttpConnector::new();
        match url.protocol {
            Protocol::Http => {
                let mut tunnel = Tunnel::new(server, connector);
                if let Some((username, password)) = &url.auth {
                    let mut auth = HeaderValue::from_str(&format!(
                        "Basic {}",
                        STANDARD.encode(format!("{username}:{password}"))
                    ))
                    .context("invalid HTTP proxy credentials")?;
                    auth.set_sensitive(true);
                    tunnel = tunnel.with_auth(auth);
                }
                Ok(Self::Http(tunnel))
            }
            Protocol::Socks5 => {
                let mut socks = SocksV5::new(server, connector);
                if let Some((username, password)) = &url.auth {
                    socks = socks.with_auth(username.clone(), password.clone());
                }
                Ok(Self::Socks5(socks))
            }
        }
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
