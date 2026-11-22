use super::{Connector, Mcp, Session, bearer, binding, get_json, protocol};
use crate::{
    net,
    security::{hash, now, pkce, token},
};
use anyhow::{Context, Result, ensure};
use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Serialize, Deserialize)]
pub(super) struct OAuthFlow {
    connector: String,
    client_id: String,
    client_secret: Option<String>,
    token_url: String,
    verifier: String,
    resource: String,
    #[serde(default)]
    client_auth: String,
}

#[derive(Serialize, Deserialize)]
pub(super) struct OAuthToken {
    pub(super) flow: OAuthFlow,
    pub(super) access_token: String,
    pub(super) refresh_token: Option<String>,
    pub(super) expires: i64,
}

impl Mcp {
    pub async fn oauth_start(
        &self,
        user: &str,
        connector: &str,
        client_id: Option<&str>,
        client_secret: Option<&str>,
    ) -> Result<String> {
        let connector = self.owned(user, connector).await?;
        let endpoint = net::https(&connector.endpoint)?;
        let probe = protocol::probe(&endpoint, &connector.transport).await?;
        ensure!(
            probe.status() == StatusCode::UNAUTHORIZED,
            "this MCP did not request OAuth; try its API-token setup"
        );
        let challenge = probe
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let mut fallback = endpoint.clone();
        fallback.set_path(&format!(
            "/.well-known/oauth-protected-resource{}",
            endpoint.path().trim_end_matches('/')
        ));
        fallback.set_query(None);
        let metadata_url = challenge_value(challenge, "resource_metadata")
            .map(net::https)
            .transpose()?
            .unwrap_or(fallback);
        ensure!(
            metadata_url.origin() == endpoint.origin(),
            "resource metadata is on an unexpected origin"
        );
        let resource = get_json(&metadata_url).await?;
        let resource_url = net::https(
            resource["resource"]
                .as_str()
                .context("missing resource identifier")?,
        )?;
        ensure!(
            resource_url == endpoint,
            "OAuth resource does not match MCP endpoint"
        );
        let issuer = net::https(
            resource["authorization_servers"][0]
                .as_str()
                .context("missing authorization server")?,
        )?;
        let mut metadata = issuer.clone();
        metadata.set_path(&format!(
            "/.well-known/oauth-authorization-server{}",
            issuer.path().trim_end_matches('/')
        ));
        let auth = match get_json(&metadata).await {
            Ok(value) => value,
            Err(_) => {
                metadata.set_path(&format!(
                    "{}/.well-known/openid-configuration",
                    issuer.path().trim_end_matches('/')
                ));
                get_json(&metadata).await?
            }
        };
        ensure!(
            auth["issuer"]
                .as_str()
                .and_then(|s| net::https(s).ok())
                .as_ref()
                == Some(&issuer),
            "OAuth issuer mismatch"
        );
        ensure!(
            auth["code_challenge_methods_supported"]
                .as_array()
                .is_some_and(|methods| methods.iter().any(|m| m == "S256")),
            "OAuth server must support PKCE S256"
        );
        let mut authorize = net::https(
            auth["authorization_endpoint"]
                .as_str()
                .context("missing authorization endpoint")?,
        )?;
        let token_url = net::https(
            auth["token_endpoint"]
                .as_str()
                .context("missing token endpoint")?,
        )?;
        for url in [&authorize, &token_url] {
            ensure!(
                url.origin() == issuer.origin(),
                "OAuth endpoints must belong to the advertised issuer"
            );
        }
        let redirect = self.origin.join("/oauth/mcp")?;
        let methods = auth["token_endpoint_auth_methods_supported"].as_array();
        let supports =
            |method: &str| methods.is_none_or(|methods| methods.iter().any(|item| item == method));
        let client_auth = if client_secret.is_none() && supports("none") {
            "none"
        } else if supports("client_secret_post") {
            "client_secret_post"
        } else if supports("client_secret_basic") {
            "client_secret_basic"
        } else {
            anyhow::bail!(
                "authorization server requires an unsupported client authentication method"
            )
        };
        let (client_id, client_secret) = if let Some(id) = client_id.filter(|id| !id.is_empty()) {
            if client_auth != "none" {
                ensure!(client_secret.is_some(), "OAuth client secret required");
            }
            (id.to_owned(), client_secret.map(str::to_owned))
        } else if auth["client_id_metadata_document_supported"] == true && supports("none") {
            ensure!(
                client_secret.is_none(),
                "provide a matching OAuth client ID"
            );
            (
                self.origin.join("/oauth/mcp-client.json")?.to_string(),
                None,
            )
        } else {
            ensure!(
                client_secret.is_none(),
                "provide a matching OAuth client ID"
            );
            let registration = net::https(
                auth["registration_endpoint"]
                    .as_str()
                    .or_else(|| challenge_value(challenge, "registrationUrl"))
                    .context("server requires a pre-registered OAuth client ID")?,
            )?;
            ensure!(
                registration.origin() == issuer.origin(),
                "unexpected OAuth registration origin"
            );
            let response=net::request(Method::POST,&registration).await?.json(&json!({
                "client_name":"TinyBot","redirect_uris":[redirect.as_str()], "token_endpoint_auth_method":client_auth,
                "grant_types":["authorization_code","refresh_token"],"response_types":["code"]
            })).send().await?.error_for_status()?;
            let value: Value = serde_json::from_slice(&net::body(response, 65536).await?)?;
            (
                value["client_id"]
                    .as_str()
                    .context("OAuth registration omitted a client ID")?
                    .to_owned(),
                value["client_secret"].as_str().map(str::to_owned),
            )
        };
        let state = token();
        let verifier = token();
        {
            let mut q = authorize.query_pairs_mut();
            q.append_pair("client_id", &client_id)
                .append_pair("redirect_uri", redirect.as_str())
                .append_pair("response_type", "code")
                .append_pair("state", &state)
                .append_pair("code_challenge", &pkce(&verifier))
                .append_pair("code_challenge_method", "S256")
                .append_pair("resource", resource_url.as_str());
            if let Some(scope) = challenge_value(challenge, "scope") {
                q.append_pair("scope", scope);
            }
        }
        let flow = OAuthFlow {
            connector: connector.id,
            client_id,
            client_secret,
            token_url: token_url.to_string(),
            verifier,
            resource: resource_url.to_string(),
            client_auth: client_auth.into(),
        };
        let state_hash = hash(&state);
        let secret = self
            .vault
            .seal(&format!("oauth:{state_hash}"), &serde_json::to_vec(&flow)?)?;
        sqlx::query(
            "INSERT INTO pending(id,user,kind,payload,expires) VALUES(?,?,'mcp-oauth',?,?)",
        )
        .bind(state_hash)
        .bind(user)
        .bind(secret)
        .bind(now() + 600)
        .execute(&self.db)
        .await?;
        Ok(authorize.to_string())
    }

    pub async fn oauth_finish(&self, state: &str, code: Option<&str>) -> Result<()> {
        let state_hash = hash(state);
        let (user, payload): (String, String) = sqlx::query_as(
            "DELETE FROM pending WHERE id=? AND kind='mcp-oauth'
            AND expires>=? RETURNING user,payload",
        )
        .bind(&state_hash)
        .bind(now())
        .fetch_optional(&self.db)
        .await?
        .context("OAuth state expired or was consumed")?;
        let Some(code) = code else {
            crate::store::notify(
                &self.db,
                &user,
                "MCP authorization was declined. You can reconnect from the Mini App.",
                Some("mini"),
            )
            .await?;
            return Ok(());
        };
        let flow: OAuthFlow =
            serde_json::from_slice(&self.vault.open(&format!("oauth:{state_hash}"), &payload)?)?;
        let redirect = self.origin.join("/oauth/mcp")?;
        let next = exchange(
            &flow,
            &[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", redirect.as_str()),
                ("code_verifier", &flow.verifier),
            ],
        )
        .await?;
        let connector = self.owned(&user, &flow.connector).await?;
        let mut session = Session::open(
            &connector.endpoint,
            &connector.transport,
            bearer(&next.access_token)?,
        )
        .await?;
        session.tools().await?;
        let oauth = OAuthToken {
            flow,
            access_token: next.access_token,
            refresh_token: next.refresh_token,
            expires: next.expires,
        };
        self.store_oauth(&connector, &oauth).await?;
        self.wake_connected(&user, &connector.id).await?;
        crate::store::notify(
            &self.db,
            &user,
            &format!(
                "{} is connected. You can use it from your Chief.",
                connector.name
            ),
            Some("mini"),
        )
        .await?;
        Ok(())
    }

    pub(super) async fn store_oauth(
        &self,
        connector: &Connector,
        oauth: &OAuthToken,
    ) -> Result<()> {
        let sealed = self
            .vault
            .seal(&binding(connector), &serde_json::to_vec(oauth)?)?;
        let updated=sqlx::query("UPDATE connectors SET auth='oauth',credential=? WHERE id=? AND owner=? AND credential IS ?")
            .bind(sealed).bind(&connector.id).bind(&connector.owner).bind(&connector.credential).execute(&self.db).await?;
        ensure!(
            updated.rows_affected() == 1,
            "connector changed during authorization"
        );
        Ok(())
    }
}

fn challenge_value<'a>(header: &'a str, key: &str) -> Option<&'a str> {
    let (_, attributes) = header.split_once(' ')?;
    attributes
        .split(',')
        .filter_map(|entry| entry.trim().split_once('='))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case(key))
        .map(|(_, value)| value.trim().trim_matches('"'))
}

pub(super) struct ReceivedToken {
    pub(super) access_token: String,
    pub(super) refresh_token: Option<String>,
    pub(super) expires: i64,
}

pub(super) async fn exchange(flow: &OAuthFlow, grant: &[(&str, &str)]) -> Result<ReceivedToken> {
    let mut params = grant.to_vec();
    params.extend([
        ("client_id", flow.client_id.as_str()),
        ("resource", flow.resource.as_str()),
    ]);
    if let Some(secret) = &flow.client_secret
        && matches!(flow.client_auth.as_str(), "" | "client_secret_post")
    {
        params.push(("client_secret", secret));
    }
    let mut request = net::request(Method::POST, &net::https(&flow.token_url)?).await?;
    if flow.client_auth == "client_secret_basic" {
        request = request.basic_auth(&flow.client_id, flow.client_secret.as_deref());
    }
    let response = request.form(&params).send().await?;
    ensure!(response.status().is_success(), "MCP token exchange failed");
    let value: Value = serde_json::from_slice(&net::body(response, 65536).await?)?;
    let access = value["access_token"]
        .as_str()
        .filter(|v| !v.is_empty() && v.len() <= 16384)
        .context("missing OAuth access token")?;
    Ok(ReceivedToken {
        access_token: access.into(),
        refresh_token: value["refresh_token"].as_str().map(str::to_owned),
        expires: now() + value["expires_in"].as_i64().unwrap_or(3600).clamp(1, 86400),
    })
}
