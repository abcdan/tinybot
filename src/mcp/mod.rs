mod discovery;
pub(crate) mod docblock;
mod oauth;
mod policy;
mod protocol;
mod stdio;

pub use discovery::Candidate;

use crate::{
    net,
    security::{Vault, clip, hash, id, now},
};
use anyhow::{Context, Result, ensure};
use oauth::{OAuthToken, exchange};
use protocol::Session;
use reqwest::{
    Method,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{FromRow, SqlitePool};
use tokio::sync::Mutex;
use url::Url;

const LIMIT: usize = 512 * 1024;

#[cfg(test)]
type DocblockFixture = Box<dyn Fn(&str, &Value) -> Result<Value> + Send + Sync>;

#[derive(Clone, FromRow, Serialize)]
pub struct Connector {
    pub id: String,
    pub owner: String,
    pub name: String,
    pub endpoint: String,
    pub auth: String,
    pub transport: String,
    pub kind: String,
    #[serde(skip)]
    pub approval_epoch: i64,
    #[serde(skip)]
    pub credential: Option<String>,
}

pub struct Mcp {
    db: SqlitePool,
    vault: Vault,
    origin: Url,
    refresh: Mutex<()>,
    policy: policy::Policy,
    #[cfg(test)]
    docblock_fixture: Option<DocblockFixture>,
}

impl Mcp {
    pub fn new(db: SqlitePool, vault: Vault, origin: Url) -> Self {
        Self {
            db,
            vault,
            origin,
            refresh: Mutex::new(()),
            policy: policy::Policy::load(),
            #[cfg(test)]
            docblock_fixture: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn set_docblock_fixture(
        &mut self,
        fixture: impl Fn(&str, &Value) -> Result<Value> + Send + Sync + 'static,
    ) {
        self.docblock_fixture = Some(Box::new(fixture));
    }

    pub async fn prepare(&self, user: &str, candidate: &Candidate) -> Result<String> {
        self.prepare_as(user, candidate, "mcp").await
    }

    pub async fn prepare_anything_mcp(&self, user: &str, endpoint: &str) -> Result<String> {
        let mut url = net::https(endpoint)?;
        ensure!(url.query().is_none(), "MCP endpoint cannot contain a query");
        // AnythingMCP advertises the endpoint without a trailing slash in its OAuth metadata.
        let path = url.path().trim_end_matches('/').to_owned();
        url.set_path(&path);
        let host = url.host_str().context("missing MCP hostname")?;
        let host_without_final_dot = host.trim_end_matches('.');
        ensure!(
            host_without_final_dot != "anythingmcp.com"
                && !host_without_final_dot.ends_with(".anythingmcp.com"),
            "Use the HTTPS endpoint of your own AnythingMCP gateway"
        );
        let path = url.path();
        let name = if path == "/mcp" {
            format!("AnythingMCP · {host}")
        } else {
            format!("AnythingMCP · {host}{path}")
        };
        let name = if name.len() > 100 {
            format!("{} · {}", clip(&name, 80), &hash(url.as_str())[..12])
        } else {
            name
        };
        let candidate = Candidate {
            name,
            endpoint: url.to_string(),
            transport: discovery::HTTP.into(),
            publisher: None,
        };
        self.prepare_as(user, &candidate, "anythingmcp").await
    }

    async fn prepare_as(&self, user: &str, candidate: &Candidate, kind: &str) -> Result<String> {
        let endpoint = candidate.endpoint()?;
        let created = id();
        let value: (String,) = sqlx::query_as("INSERT INTO connectors(id,owner,name,endpoint,auth,transport,kind) VALUES(?,?,?,?,'pending',?,?)
            ON CONFLICT(owner,name) DO UPDATE SET kind=CASE WHEN excluded.kind='anythingmcp' THEN excluded.kind ELSE connectors.kind END
            WHERE connectors.endpoint=excluded.endpoint RETURNING id")
            .bind(&created).bind(user).bind(clip(&candidate.name,100)).bind(endpoint).bind(&candidate.transport).bind(kind)
            .fetch_one(&self.db).await?;
        Ok(value.0)
    }

    pub async fn owned(&self, user: &str, connector: &str) -> Result<Connector> {
        sqlx::query_as("SELECT * FROM connectors WHERE owner=? AND id=?")
            .bind(user)
            .bind(connector)
            .fetch_optional(&self.db)
            .await?
            .context("connector unavailable")
    }

    pub async fn available(&self, user: &str, agent: Option<&str>) -> Result<Vec<Connector>> {
        let rows = if let Some(agent) = agent {
            sqlx::query_as(
                "SELECT DISTINCT c.* FROM connectors c JOIN agents a ON a.id=?
                WHERE c.auth<>'pending' AND (c.owner=? AND a.owner=? OR EXISTS(
                SELECT 1 FROM grants g JOIN connector_grants cg ON cg.agent=g.agent
                WHERE g.member=? AND g.agent=a.id AND cg.connector=c.id AND a.owner=c.owner))",
            )
            .bind(agent)
            .bind(user)
            .bind(user)
            .bind(user)
            .fetch_all(&self.db)
            .await?
        } else {
            sqlx::query_as("SELECT * FROM connectors WHERE owner=? AND auth<>'pending'")
                .bind(user)
                .fetch_all(&self.db)
                .await?
        };
        Ok(rows)
    }

    pub async fn save_api(
        &self,
        user: &str,
        connector: &str,
        key: &str,
        header: &str,
        prefix: &str,
    ) -> Result<()> {
        let connector = self.owned(user, connector).await?;
        let credential = ApiCredential {
            header: header.into(),
            prefix: prefix.into(),
            key: key.into(),
        };
        if connector.transport == discovery::STDIO {
            stdio::call(&connector, Some(key), "tools/list", json!({})).await?;
        } else {
            Session::open(
                &connector.endpoint,
                &connector.transport,
                credential.headers()?,
            )
            .await?
            .tools()
            .await?;
        }
        let sealed = self
            .vault
            .seal(&binding(&connector), &serde_json::to_vec(&credential)?)?;
        sqlx::query("UPDATE connectors SET auth='api',credential=?,approval_epoch=approval_epoch+1 WHERE id=? AND owner=?")
            .bind(sealed)
            .bind(&connector.id)
            .bind(user)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    pub async fn save_public(&self, user: &str, id: &str) -> Result<()> {
        let connector = self.owned(user, id).await?;
        ensure!(
            connector.kind != "anythingmcp",
            "Sign in or use a personal MCP API key"
        );
        if connector.transport == discovery::STDIO {
            stdio::call(&connector, None, "tools/list", json!({})).await?;
        } else {
            Session::open(&connector.endpoint, &connector.transport, HeaderMap::new())
                .await?
                .tools()
                .await?;
        }
        sqlx::query("UPDATE connectors SET auth='public',credential=NULL,approval_epoch=approval_epoch+1 WHERE id=? AND owner=?")
            .bind(id)
            .bind(user)
            .execute(&self.db)
            .await?;
        self.wake_connected(user, id).await
    }

    pub async fn wake_connected(&self, user: &str, id: &str) -> Result<()> {
        self.wake_connected_with_access(user, id, None).await
    }

    async fn wake_connected_with_access(
        &self,
        user: &str,
        id: &str,
        docblock_access: Option<&str>,
    ) -> Result<()> {
        let mut tx = self.db.begin_with("BEGIN IMMEDIATE").await?;
        let matches: Vec<(String, String, i64)> = sqlx::query_as(
            "SELECT p.id,p.task,p.revision FROM pending p
            JOIN tasks t ON t.id=p.task AND t.revision=p.revision AND t.state='waiting'
            WHERE p.user=? AND p.kind='connector-setup' AND p.state='waiting' AND p.expires>=?
            AND json_extract(p.payload,'$.connector')=?",
        )
        .bind(user)
        .bind(now())
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
        let message = match docblock_access {
            Some("write") => format!(
                "The user connected Docblock connector {id} with write access. Continue the request."
            ),
            Some("propose") => format!(
                "The user connected Docblock connector {id} with propose-only access. Do not publish. Explain that changes require Docblock review."
            ),
            Some(_) => format!(
                "The user connected Docblock connector {id} with limited access. Do not request another sign-in in this turn. Explain the grant and that the user can sign in again in Connections."
            ),
            None => format!("The user connected MCP connector {id}. It is now ready."),
        };
        for (pending, task, revision) in matches {
            sqlx::query("UPDATE pending SET state='done' WHERE id=?")
                .bind(pending)
                .execute(&mut *tx)
                .await?;
            sqlx::query("INSERT INTO events(task,role,body) VALUES(?,'tool',?)")
                .bind(&task)
                .bind(&message)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE tasks SET state='queued',updated=? WHERE id=? AND revision=?")
                .bind(now())
                .bind(task)
                .bind(revision)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn session(&self, connector: &Connector) -> Result<Session> {
        let _refresh = self.refresh.lock().await;
        let connector = self.current_connection(connector).await?;
        if connector.auth == "public" {
            drop(_refresh);
            return Session::open(&connector.endpoint, &connector.transport, HeaderMap::new())
                .await;
        }
        let secret = self.vault.open(
            &binding(&connector),
            connector
                .credential
                .as_deref()
                .context("connector needs authorization")?,
        )?;
        let headers = match connector.auth.as_str() {
            "api" => ApiCredential::decode(secret)?.headers()?,
            "oauth" => {
                let mut oauth: OAuthToken = serde_json::from_slice(&secret)?;
                if oauth.expires <= now() + 45 {
                    let refresh = oauth
                        .refresh_token
                        .as_deref()
                        .context("OAuth reconnect required")?;
                    let next = exchange(
                        &oauth.flow,
                        &[("grant_type", "refresh_token"), ("refresh_token", refresh)],
                    )
                    .await
                    .context("OAuth reconnect required")?;
                    oauth.access_token = next.access_token;
                    oauth.refresh_token = next.refresh_token.or(oauth.refresh_token);
                    oauth.expires = next.expires;
                    if next.scopes.is_some() {
                        oauth.scopes = next.scopes;
                    }
                    self.store_oauth(&connector, &oauth, false).await?;
                }
                bearer(&oauth.access_token)?
            }
            _ => anyhow::bail!("connector needs authorization"),
        };
        drop(_refresh);
        Session::open(&connector.endpoint, &connector.transport, headers).await
    }

    async fn current_connection(&self, connector: &Connector) -> Result<Connector> {
        let current = self.owned(&connector.owner, &connector.id).await?;
        ensure!(
            current.approval_epoch == connector.approval_epoch,
            "connector authorization changed"
        );
        Ok(current)
    }

    pub async fn list_tools(&self, connector: &Connector) -> Result<Value> {
        if connector.transport == discovery::STDIO {
            return Ok(self.stdio(connector, "tools/list", json!({})).await?["tools"].clone());
        }
        self.session(connector).await?.tools().await
    }

    pub async fn call(&self, connector: &Connector, tool: &str, args: &Value) -> Result<Value> {
        #[cfg(test)]
        if docblock::is_endpoint(&connector.endpoint)
            && let Some(fixture) = &self.docblock_fixture
        {
            let current = self.current_connection(connector).await?;
            ensure!(current.auth == "oauth", "fixture authorization changed");
            return fixture(tool, args);
        }
        if connector.transport == discovery::STDIO {
            return self
                .stdio(
                    connector,
                    "tools/call",
                    json!({"name":tool,"arguments":args}),
                )
                .await;
        }
        self.session(connector)
            .await?
            .rpc("tools/call", json!({"name":tool,"arguments":args}))
            .await
    }
}

#[derive(Serialize, Deserialize)]
struct ApiCredential {
    header: String,
    prefix: String,
    key: String,
}

impl ApiCredential {
    fn decode(secret: Vec<u8>) -> Result<Self> {
        if let Ok(value) = serde_json::from_slice(&secret) {
            return Ok(value);
        }
        Ok(Self {
            header: "authorization".into(),
            prefix: "Bearer".into(),
            key: String::from_utf8(secret)?,
        })
    }

    fn headers(&self) -> Result<HeaderMap> {
        let header = self.header.to_ascii_lowercase();
        ensure!(
            header == "authorization" || header == "api-key" || header.starts_with("x-"),
            "use Authorization, API-Key, or a vendor X- header"
        );
        ensure!(
            self.key.len() <= 8192
                && !self.key.is_empty()
                && self.prefix.len() <= 32
                && self
                    .prefix
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
            "invalid API token settings"
        );
        let value = if self.prefix.is_empty() {
            self.key.clone()
        } else {
            format!("{} {}", self.prefix, self.key)
        };
        let mut value = HeaderValue::from_str(&value)?;
        value.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(HeaderName::from_bytes(header.as_bytes())?, value);
        Ok(headers)
    }
}

fn bearer(key: &str) -> Result<HeaderMap> {
    ApiCredential {
        header: "authorization".into(),
        prefix: "Bearer".into(),
        key: key.into(),
    }
    .headers()
}

fn binding(c: &Connector) -> String {
    format!("connector:{}:{}", c.owner, c.id)
}

async fn get_json(url: &Url) -> Result<Value> {
    let response = net::request(Method::GET, url)
        .await?
        .send()
        .await?
        .error_for_status()?;
    ensure!(
        response.status().is_success(),
        "unexpected metadata response"
    );
    Ok(serde_json::from_slice(&net::body(response, LIMIT).await?)?)
}
