use super::upstream::{ProxyUrl, Upstream};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::{
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Read, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
};
use tokio::net::TcpStream;
use uuid::Uuid;

const SETTINGS_PATH: &str = "/browser-proxy/settings.json";
const MAX_URL_BYTES: usize = 2048;
const MAX_FILE_BYTES: usize = 8192;

#[derive(Serialize)]
pub struct BrowserProxyStatus {
    pub mode: &'static str,
    pub endpoint: Option<String>,
    pub authenticated: bool,
    pub deployment_configured: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
enum Preference {
    Direct,
    Proxy { url: String },
}

pub fn browser_proxy_status() -> Result<BrowserProxyStatus> {
    let deployment =
        std::env::var_os("TINYBOT_BROWSER_PROXY_DEPLOYED").is_some_and(|value| value == "1");
    status(Path::new(SETTINGS_PATH), deployment)
}

pub fn validate_browser_proxy(url: &str) -> Result<()> {
    parse(url.trim()).map(|_| ())
}

pub fn set_browser_proxy(url: Option<&str>) -> Result<()> {
    set(Path::new(SETTINGS_PATH), url)
}

pub fn clear_browser_proxy() -> Result<()> {
    clear(Path::new(SETTINGS_PATH))
}

fn status(path: &Path, deployment_configured: bool) -> Result<BrowserProxyStatus> {
    let (mode, proxy) = match load(path)? {
        None => ("deployment", None),
        Some(Preference::Direct) => ("direct", None),
        Some(Preference::Proxy { url }) => ("proxy", Some(parse(&url)?)),
    };
    Ok(BrowserProxyStatus {
        mode,
        endpoint: proxy.as_ref().map(ProxyUrl::endpoint),
        authenticated: proxy.as_ref().is_some_and(ProxyUrl::authenticated),
        deployment_configured,
    })
}

fn set(path: &Path, url: Option<&str>) -> Result<()> {
    let preference = match url.map(str::trim) {
        Some(url) => {
            parse(url)?;
            Preference::Proxy { url: url.into() }
        }
        None => Preference::Direct,
    };
    let data = serde_json::to_vec(&preference).context("encode browser proxy settings")?;
    let parent = path
        .parent()
        .context("missing browser proxy settings directory")?;
    let scratch = parent.join(format!(".{}.tmp", Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&scratch)
        .context("browser proxy settings storage is unavailable")?;
    let result = (|| -> Result<()> {
        file.write_all(&data)?;
        file.sync_all()?;
        fs::rename(&scratch, path)?;
        sync_directory(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(scratch);
    }
    result.context("save browser proxy settings")
}

fn clear(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => sync_directory(
            path.parent()
                .context("missing browser proxy settings directory")?,
        ),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
    .context("clear browser proxy settings")
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn load(path: &Path) -> Result<Option<Preference>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read browser proxy settings"),
    };
    let mut bytes = Vec::new();
    file.take((MAX_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .context("read browser proxy settings")?;
    ensure!(
        bytes.len() <= MAX_FILE_BYTES,
        "browser proxy settings exceed size limit"
    );
    let value = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("invalid stored browser proxy settings"))?;
    Ok(Some(value))
}

fn parse(url: &str) -> Result<ProxyUrl> {
    ensure!(
        url.len() <= MAX_URL_BYTES,
        "browser proxy URL exceeds size limit"
    );
    ProxyUrl::public(url)
}

pub(super) struct Routes {
    path: PathBuf,
    deployment: Result<Option<Upstream>>,
}

impl Routes {
    pub(super) fn configured() -> Self {
        Self {
            path: PathBuf::from(SETTINGS_PATH),
            deployment: Upstream::configured(),
        }
    }

    pub(super) async fn connect(&self, addresses: &[SocketAddr]) -> Result<TcpStream> {
        let path = self.path.clone();
        // The atomic rename makes each new CONNECT see a completed setting.
        let preference = tokio::task::spawn_blocking(move || load(&path)).await??;
        match preference {
            Some(Preference::Proxy { url }) => parse(&url)?.connect(addresses).await,
            Some(Preference::Direct) => Ok(TcpStream::connect(addresses).await?),
            None => match self
                .deployment
                .as_ref()
                .map_err(|_| anyhow::anyhow!("invalid deployment browser proxy setting"))?
            {
                Some(proxy) => proxy.connect(addresses).await,
                None => Ok(TcpStream::connect(addresses).await?),
            },
        }
    }

    #[cfg(test)]
    pub(super) fn fixture(path: PathBuf, proxy: Option<Upstream>) -> Self {
        Self {
            path,
            deployment: Ok(proxy),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let directory = std::env::temp_dir().join(format!("browser-proxy-{}", Uuid::new_v4()));
            fs::create_dir(&directory).unwrap();
            Self(directory)
        }

        fn path(&self) -> PathBuf {
            self.0.join("settings.json")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn overrides_are_private_and_status_redacts_credentials() {
        let fixture = Fixture::new();
        let path = fixture.path();
        let initial = status(&path, true).unwrap();
        assert_eq!(initial.mode, "deployment");
        assert!(initial.deployment_configured);
        assert!(initial.endpoint.is_none());

        set(
            &path,
            Some("socks5://fixture-user:fixture-password@proxy.example:1080"),
        )
        .unwrap();
        let current = status(&path, true).unwrap();
        assert_eq!(current.mode, "proxy");
        assert_eq!(
            current.endpoint.as_deref(),
            Some("socks5://proxy.example:1080")
        );
        assert!(current.authenticated);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!serde_json::to_string(&current).unwrap().contains("fixture"));

        assert!(
            set(
                &path,
                Some("http://fixture-user:fixture-password@127.0.0.1")
            )
            .is_err()
        );
        assert_eq!(status(&path, true).unwrap().mode, "proxy");
        set(&path, None).unwrap();
        let direct = status(&path, true).unwrap();
        assert_eq!(direct.mode, "direct");
        assert!(direct.endpoint.is_none());
        assert!(!direct.authenticated);
        assert!(direct.deployment_configured);

        clear(&path).unwrap();
        assert_eq!(status(&path, true).unwrap().mode, "deployment");
        assert!(!path.exists());
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 0);
    }

    #[test]
    fn telegram_overrides_require_public_proxy_endpoints() {
        for url in [
            "http://127.0.0.1:3128",
            "http://169.254.169.254",
            "socks5://127.000.000.001:1080",
            "socks5://[::ffff:127.0.0.1]:1080",
            "socks5://proxy.localhost.:1080",
            "http://proxy.local",
            "http://egress",
        ] {
            assert!(validate_browser_proxy(url).is_err(), "{url}");
        }
        for url in [
            "socks5://1.1.1.1:1080",
            "socks5://[2606:4700:4700::1111]:1080",
            "http://proxy.example",
        ] {
            assert!(validate_browser_proxy(url).is_ok(), "{url}");
        }
    }

    #[tokio::test]
    async fn listener_reloads_each_new_connect_without_a_restart() {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };

        let relay = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let deployment =
            Upstream::parse(&format!("http://{}", relay.local_addr().unwrap())).unwrap();
        let upstream = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut socket, _) = relay.accept().await.unwrap();
                let mut bytes = Vec::new();
                while !bytes.ends_with(b"\r\n\r\n") {
                    bytes.push(socket.read_u8().await.unwrap());
                }
                assert!(bytes.starts_with(b"CONNECT 1.1.1.1:443 HTTP/1.1\r\n"));
                socket
                    .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    .await
                    .unwrap();
            }
        });
        let fixture = Fixture::new();
        let path = fixture.path();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let routes = Routes::fixture(path.clone(), Some(deployment));
        let worker = tokio::spawn(async move {
            for _ in 0..4 {
                super::super::handle(listener.accept().await.unwrap().0, &routes)
                    .await
                    .unwrap();
            }
        });
        let request = async || {
            let mut client = TcpStream::connect(address).await.unwrap();
            client
                .write_all(b"CONNECT 1.1.1.1:443 HTTP/1.1\r\n\r\n")
                .await
                .unwrap();
            client.shutdown().await.unwrap();
            let mut response = String::new();
            client.read_to_string(&mut response).await.unwrap();
            response
        };

        assert!(request().await.starts_with("HTTP/1.1 200 "));
        fs::write(&path, "invalid stored data").unwrap();
        assert!(request().await.starts_with("HTTP/1.1 502 "));
        // Even a malformed file on the volume cannot turn a private address into a live proxy.
        fs::write(&path, br#"{"mode":"proxy","url":"http://127.0.0.1"}"#).unwrap();
        assert!(request().await.starts_with("HTTP/1.1 502 "));
        clear(&path).unwrap();
        assert!(request().await.starts_with("HTTP/1.1 200 "));
        upstream.await.unwrap();
        worker.await.unwrap();
    }

    #[test]
    fn storage_and_invalid_config_errors_are_bounded_and_redacted() {
        let fixture = Fixture::new();
        let path = fixture.path();
        let absent = fixture.0.join("missing").join("settings.json");
        assert_eq!(status(&absent, false).unwrap().mode, "deployment");
        assert!(
            format!("{:#}", set(&absent, None).unwrap_err())
                .contains("browser proxy settings storage is unavailable")
        );
        clear(&absent).unwrap();

        fs::write(&path, br#"{"mode":"fixture-user:fixture-password"}"#).unwrap();
        let error = status(&path, false).err().unwrap();
        assert_eq!(error.to_string(), "invalid stored browser proxy settings");
        fs::write(&path, vec![b' '; MAX_FILE_BYTES + 1]).unwrap();
        assert_eq!(
            status(&path, false).err().unwrap().to_string(),
            "browser proxy settings exceed size limit"
        );
    }
}
