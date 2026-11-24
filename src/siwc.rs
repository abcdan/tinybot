use crate::{
    net,
    security::{now, pkce, token},
};
use anyhow::{Context, Result, anyhow, ensure};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashMap, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use url::Url;

const RESOURCE: &str = "https://api.openai.com/v1";
const ISSUER: &str = "https://auth.openai.com";
const TOKEN_URL: &str = "https://auth.openai.com/api/accounts/oauth/token";
const DIRECT: &str = "chatgpt.tokens.use.direct";

#[derive(Serialize, Deserialize)]
pub struct Profile {
    pub client_id: String,
    pub access_token: String,
    refresh_token: String,
    id_token: String,
    pub subject: String,
    pub expires: i64,
}

#[derive(Deserialize)]
struct Tokens {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    scope: Option<String>,
    expires_in: i64,
}

async fn exchange(params: &[(&str, &str)]) -> Result<Tokens> {
    let endpoint = Url::parse(TOKEN_URL)?;
    let response = net::request(reqwest::Method::POST, &endpoint)
        .await?
        .form(params)
        .send()
        .await?
        .error_for_status()
        .context("OpenAI declined the token request")?;
    Ok(serde_json::from_slice(&net::body(response, 65536).await?)?)
}

impl Profile {
    pub async fn refresh(&mut self) -> Result<()> {
        let next = exchange(&[
            ("grant_type", "refresh_token"),
            ("client_id", &self.client_id),
            ("refresh_token", &self.refresh_token),
            ("resource", RESOURCE),
        ])
        .await?;
        ensure!(
            next.scope
                .as_deref()
                .is_none_or(|scope| scope.split_whitespace().any(|v| v == DIRECT)),
            "ChatGPT plan permission was removed"
        );
        if let Some(id_token) = next.id_token {
            ensure!(
                claims(&id_token, &self.client_id, None).await? == self.subject,
                "ChatGPT account changed"
            );
            self.id_token = id_token;
        }
        if let Some(refresh) = next.refresh_token {
            self.refresh_token = refresh;
        }
        self.access_token = next.access_token;
        self.expires = now() + next.expires_in.clamp(1, 86400);
        Ok(())
    }

    pub async fn validate_import(&mut self) -> Result<()> {
        ensure!(
            self.client_id.starts_with("oaiapp_"),
            "invalid registration"
        );
        ensure!(
            claims(&self.id_token, &self.client_id, None).await? == self.subject,
            "account verification failed"
        );
        self.refresh().await
    }
}

async fn claims(id_token: &str, client: &str, nonce: Option<&str>) -> Result<String> {
    let config = net::request(
        reqwest::Method::GET,
        &Url::parse(&format!("{ISSUER}/.well-known/openid-configuration"))?,
    )
    .await?
    .send()
    .await?
    .error_for_status()?;
    let config: Value = serde_json::from_slice(&net::body(config, 131072).await?)?;
    ensure!(config["issuer"] == ISSUER, "unexpected OpenAI issuer");
    let url = net::https(
        config["jwks_uri"]
            .as_str()
            .context("missing OpenAI signing keys")?,
    )?;
    ensure!(
        url.host_str() == Some("auth.openai.com"),
        "unexpected signing key host"
    );
    let keys = net::request(reqwest::Method::GET, &url)
        .await?
        .send()
        .await?
        .error_for_status()?;
    let keys: JwkSet = serde_json::from_slice(&net::body(keys, 131072).await?)?;
    let header = decode_header(id_token)?;
    ensure!(
        header.alg == Algorithm::RS256,
        "unexpected OpenAI ID token signature type"
    );
    let key = keys
        .find(&header.kid.context("missing signing key id")?)
        .context("unknown signing key")?;
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_issuer(&[ISSUER]);
    validation.set_audience(&[client]);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    let data = decode::<Value>(id_token, &DecodingKey::from_jwk(key)?, &validation)?.claims;
    ensure!(
        nonce.is_none_or(|nonce| data["nonce"] == nonce),
        "invalid sign-in nonce"
    );
    Ok(data["sub"]
        .as_str()
        .context("missing OpenAI subject")?
        .into())
}

pub async fn connect(origin: &str) -> Result<()> {
    let mut destination = net::https(origin)?;
    ensure!(
        destination.path() == "/" && destination.query().is_none(),
        "pass only your TinyBot HTTPS origin"
    );
    eprintln!("Enter the pairing code displayed by {destination} in your TinyBot Mini App.");
    let mut pairing = String::new();
    std::io::stdin().read_line(&mut pairing)?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let redirect = format!(
        "http://127.0.0.1:{}/callback",
        listener.local_addr()?.port()
    );
    let verifier = token();
    let state = token();
    let nonce = token();
    let mut authorize = Url::parse(&format!("{ISSUER}/api/accounts/authorize"))?;
    authorize
        .query_pairs_mut()
        .append_pair("client_id", "dynamic_agent_client")
        .append_pair("agent_name_hint", "TinyBot")
        .append_pair(
            "ext_agent_host_id",
            &format!("urn:uuid:{}", crate::security::id()),
        )
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &redirect)
        .append_pair(
            "scope",
            "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct",
        )
        .append_pair("resource", RESOURCE)
        .append_pair("state", &state)
        .append_pair("nonce", &nonce)
        .append_pair("code_challenge", &pkce(&verifier))
        .append_pair("code_challenge_method", "S256");
    eprintln!(
        "Open this link in the browser on this computer to continue with ChatGPT:\n{authorize}"
    );
    let (mut socket, fields) = tokio::time::timeout(Duration::from_secs(300), async {
        loop {
            let (mut socket, _) = listener.accept().await?;
            let mut request = [0; 8192];
            let Ok(Ok(read)) =
                tokio::time::timeout(Duration::from_secs(3), socket.read(&mut request)).await
            else {
                continue;
            };
            let request = String::from_utf8_lossy(&request[..read]);
            if let Some(path) = request
                .lines()
                .next()
                .and_then(|line| line.strip_prefix("GET "))
                .and_then(|line| line.split_whitespace().next())
                .filter(|path| path.starts_with("/callback?"))
            {
                let callback = Url::parse(&redirect)?.join(path)?;
                let fields: HashMap<String, String> = callback.query_pairs().into_owned().collect();
                if fields.get("state") == Some(&state) {
                    return Ok::<_, anyhow::Error>((socket, fields));
                }
            }
            let _ = socket
                .write_all(
                    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
        }
    })
    .await??;
    let message = b"Sign-in returned to TinyBot. Check your terminal for the result.";
    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",message.len()).as_bytes()).await?;
    socket.write_all(message).await?;
    ensure!(fields.get("error").is_none(), "ChatGPT sign-in declined");
    let client = fields
        .get("client_id")
        .filter(|value| value.starts_with("oaiapp_"))
        .context("registration missing client ID")?;
    let code = fields.get("code").context("missing authorization code")?;
    let tokens = exchange(&[
        ("grant_type", "authorization_code"),
        ("client_id", client),
        ("code", code),
        ("code_verifier", &verifier),
        ("redirect_uri", &redirect),
        ("resource", RESOURCE),
    ])
    .await?;
    ensure!(
        tokens
            .scope
            .as_deref()
            .is_some_and(|scope| scope.split_whitespace().any(|word| word == DIRECT)),
        "ChatGPT plan use was not granted"
    );
    let id_token = tokens
        .id_token
        .context("OpenAI did not return an ID token")?;
    let profile = Profile {
        client_id: client.clone(),
        subject: claims(&id_token, client, Some(&nonce)).await?,
        access_token: tokens.access_token,
        refresh_token: tokens
            .refresh_token
            .context("OpenAI did not return a refresh token")?,
        id_token,
        expires: now() + tokens.expires_in.clamp(1, 86400),
    };
    destination.set_path("/api/codex/import");
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()?;
    let response = client
        .post(destination)
        .bearer_auth(pairing.trim())
        .json(&profile)
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(anyhow!(
            "The server could not import the profile. Check the pairing code and start a new sign-in."
        ));
    }
    println!("ChatGPT is connected. Return to the TinyBot Mini App.");
    Ok(())
}
