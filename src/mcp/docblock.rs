use super::{Candidate, Connector, Mcp, binding, discovery, oauth::OAuthToken};
use crate::{net, security::clip};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::{Builder, Uuid};

pub(crate) const ENDPOINT: &str = "https://mcp.docblock.dev/";
pub(crate) const ALIAS: &str = "https://docblock.dev/mcp";
pub(crate) const ORIGIN: &str = "https://docblock.dev";
pub(crate) const WRITE: &str = "notes:write";
const PROPOSE: &str = "notes:propose";
const DEFAULT_EXPIRY_DAYS: u64 = 7;
const READ_RESULT_BYTES: usize = 10_500;

pub(crate) fn is_endpoint(endpoint: &str) -> bool {
    matches!(endpoint, ENDPOINT | ALIAS)
}

pub(crate) fn reviewed_write(tool: &str) -> bool {
    matches!(
        tool,
        "write_note" | "append_note" | "update_note" | "create_share"
    )
}

impl Mcp {
    pub(crate) async fn prepare_docblock(&self, user: &str) -> Result<Connector> {
        let mut tx = self.db.begin_with("BEGIN IMMEDIATE").await?;
        let existing = sqlx::query_as(
            "SELECT * FROM connectors WHERE owner=? AND endpoint IN (?,?)
            ORDER BY auth<>'oauth', auth='pending', endpoint<>?, id LIMIT 1",
        )
        .bind(user)
        .bind(ENDPOINT)
        .bind(ALIAS)
        .bind(ENDPOINT)
        .fetch_optional(&mut *tx)
        .await?;
        let connector = match existing {
            Some(connector) => connector,
            None => {
                let candidate = Candidate {
                    name: "Docblock".into(),
                    endpoint: ENDPOINT.into(),
                    transport: discovery::HTTP.into(),
                    publisher: None,
                };
                let id = crate::security::id();
                sqlx::query_as(
                    "INSERT INTO connectors(id,owner,name,endpoint,auth,transport)
                    SELECT ?,?,CASE WHEN EXISTS(SELECT 1 FROM connectors WHERE owner=? AND name=?)
                    THEN 'Docblock · ' || ? ELSE ? END,?,'pending',? RETURNING *",
                )
                .bind(&id)
                .bind(user)
                .bind(user)
                .bind(&candidate.name)
                .bind(&id[..8])
                .bind(&candidate.name)
                .bind(candidate.endpoint()?)
                .bind(&candidate.transport)
                .fetch_one(&mut *tx)
                .await?
            }
        };
        tx.commit().await?;
        Ok(connector)
    }

    pub(crate) fn docblock_access(&self, connector: &Connector) -> Option<&'static str> {
        if !is_endpoint(&connector.endpoint) {
            return None;
        }
        if connector.auth != "oauth" {
            return Some("sign-in");
        }
        let scopes = (|| {
            let credential = self
                .vault
                .open(&binding(connector), connector.credential.as_deref()?)
                .ok()?;
            serde_json::from_slice::<OAuthToken>(&credential)
                .ok()?
                .scopes
        })();
        Some(match scopes {
            Some(scopes) if scopes.iter().any(|scope| scope == WRITE) => "write",
            Some(scopes) if scopes.iter().any(|scope| scope == PROPOSE) => "propose",
            Some(_) => "read",
            None => "reauthorize",
        })
    }

    pub(crate) async fn docblock_workspaces(&self, connector: &Connector) -> Result<Value> {
        ensure!(
            is_endpoint(&connector.endpoint) && connector.auth == "oauth",
            "Sign in to Docblock first."
        );
        let output = tool_output(&self.call(connector, "list_workspaces", &json!({})).await?)?;
        let workspaces = output["workspaces"]
            .as_array()
            .context("Docblock did not return workspaces")?;
        Ok(
            json!({"workspaces":workspaces.iter().filter_map(|workspace| {
            let id = workspace["id"].as_str()?;
            Uuid::parse_str(id).ok()?;
            Some(json!({"id":id,"name":clip(workspace["name"].as_str()?,160),"role":workspace["role"]}))
        }).collect::<Vec<_>>() }),
        )
    }

    pub(crate) async fn docblock_folders(
        &self,
        connector: &Connector,
        workspace: &str,
    ) -> Result<Vec<String>> {
        ensure!(
            matches!(self.docblock_access(connector), Some("write" | "propose")),
            "Sign in with Docblock note access first."
        );
        let available = self.docblock_workspaces(connector).await?;
        ensure!(
            available["workspaces"]
                .as_array()
                .is_some_and(
                    |entries| entries.iter().any(|entry| entry["id"] == workspace
                        && matches!(entry["role"].as_str(), Some("owner" | "editor")))
                ),
            "Choose a granted Docblock workspace you can edit."
        );
        let output = tool_output(
            &self
                .call(
                    connector,
                    "list_notes",
                    &json!({"workspace":workspace,"limit":1}),
                )
                .await?,
        )?;
        let folders = output["folders"]
            .as_array()
            .context("Docblock did not return its folders")?;
        Ok(folders
            .iter()
            .filter_map(Value::as_str)
            .filter(|path| {
                path.len() <= 240
                    && !path.is_empty()
                    && path.split('/').all(|part| {
                        part.len() <= 80
                            && part
                                .as_bytes()
                                .first()
                                .is_some_and(u8::is_ascii_alphanumeric)
                            && part.bytes().all(|b| {
                                b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b)
                            })
                    })
            })
            .take(750)
            .map(str::to_owned)
            .collect())
    }

    pub(crate) async fn docblock_note_absent(
        &self,
        connector: &Connector,
        args: &Value,
    ) -> Result<bool> {
        let result = self
            .call(
                connector,
                "read_note",
                &json!({"workspace":args["workspace"],"path":args["path"]}),
            )
            .await?;
        Ok(result["isError"] == true
            && result["content"].as_array().is_some_and(|entries| {
                entries.iter().any(|entry| {
                    entry["type"] == "text"
                        && entry["text"]
                            .as_str()
                            .is_some_and(|text| text.starts_with("not_found: "))
                })
            }))
    }

    pub(crate) async fn docblock_shares(
        &self,
        connector: &Connector,
        workspace: &str,
    ) -> Result<Value> {
        ensure!(
            self.docblock_access(connector) == Some("write"),
            "Docblock write access is required to manage links."
        );
        let output = tool_output(
            &self
                .call(connector, "list_shares", &json!({"workspace":workspace}))
                .await?,
        )?;
        let shares = output["shares"]
            .as_array()
            .context("Docblock did not return public links")?;
        let shares: Vec<_> = shares.iter().filter_map(|share| {
            if share["kind"] != "note" || share["active"] != true {
                return None;
            }
            let id = share["id"].as_str()?;
            Uuid::parse_str(id).ok()?;
            Some(json!({"id":id,"target":clip(share["target"].as_str()?,300),"expires_at":share["expiresAt"]}))
        }).collect();
        Ok(json!({"shares":shares}))
    }

    pub(crate) async fn revoke_docblock_share(
        &self,
        connector: &Connector,
        workspace: &str,
        share: &str,
    ) -> Result<()> {
        let available = self.docblock_shares(connector, workspace).await?;
        ensure!(
            available["shares"]
                .as_array()
                .is_some_and(|shares| shares.iter().any(|entry| entry["id"] == share)),
            "That Docblock note link is no longer available."
        );
        let output = tool_output(
            &self
                .call(
                    connector,
                    "revoke_share",
                    &json!({"workspace":workspace,"share_id":share}),
                )
                .await?,
        )?;
        ensure!(
            output["share"]["id"] == share && output["share"]["active"] == false,
            "Docblock did not confirm that the link was revoked."
        );
        Ok(())
    }
}

pub(crate) fn prepare_arguments(tool: &str, arguments: &mut Value, task: &str, revision: i64) {
    let Value::Object(arguments) = arguments else {
        return;
    };
    if tool == "create_share" {
        arguments
            .entry("expires_in_days")
            .or_insert(json!(DEFAULT_EXPIRY_DAYS));
    }
    if matches!(tool, "append_note" | "update_note") {
        arguments.remove("request_id");
        let seed = json!([task, revision, tool, arguments]);
        let digest = Sha256::digest(seed.to_string());
        let mut bytes = [0; 16];
        bytes.copy_from_slice(&digest[..16]);
        arguments.insert(
            "request_id".into(),
            json!(Builder::from_random_bytes(bytes).into_uuid().to_string()),
        );
    }
}

pub(crate) fn valid_expiry(days: u64) -> bool {
    (1..=365).contains(&days)
}

pub(crate) fn review(tool: &str, args: &Value) -> Result<Value> {
    let workspace = required(args, "workspace")?;
    let reference = if tool == "update_note" {
        &args["note"]
    } else {
        args
    };
    let path = reference["path"]
        .as_str()
        .filter(|value| !value.trim().is_empty());
    let id = reference["id"].as_str();
    let target = match tool {
        "write_note" => required(args, "path")?,
        _ => match (id, path) {
            (Some(id), None) if Uuid::parse_str(id).is_ok() => id,
            (None, Some(path)) => path,
            _ => anyhow::bail!("identify one Docblock note by id or path"),
        },
    };
    let (operation, change, expiry) = match tool {
        "create_share" => {
            ensure!(
                args["kind"] == "note",
                "TinyBot publishes one Docblock note at a time"
            );
            let days = args["expires_in_days"]
                .as_u64()
                .filter(|days| valid_expiry(*days))
                .context("choose a link expiry from 1 to 365 days")?;
            (
                "Publish Docblock note",
                "Anyone with the link can read this note. Later edits appear at the same link."
                    .to_owned(),
                Some(days),
            )
        }
        "write_note" => {
            let body = args["body"]
                .as_str()
                .context("provide the Markdown note body")?;
            if args["base_version"].is_null() {
                ensure!(
                    args["base_revision"].is_null(),
                    "read the note for its version and revision"
                );
            } else {
                check_base(args)?;
            }
            let operation = if args["base_version"].is_null() {
                "Create Docblock note"
            } else {
                "Replace Docblock note"
            };
            let mut change = String::new();
            if let Some(title) = args["title"].as_str() {
                change.push_str(&format!("Title: {title}\n"));
            }
            if let Some(tags) = args["tags"].as_array() {
                change.push_str(&format!(
                    "Tags: {}\n",
                    tags.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            change.push_str(body);
            (operation, change, None)
        }
        "append_note" => (
            "Append to Docblock note",
            required(args, "content")?.to_owned(),
            None,
        ),
        "update_note" => {
            check_base(args)?;
            let replacements = args["replacements"]
                .as_array()
                .filter(|items| !items.is_empty() && items.len() <= 100)
                .context("provide Docblock note replacements")?;
            let mut change = Vec::with_capacity(replacements.len());
            for entry in replacements {
                let old = required(entry, "old_text")?;
                let new = entry["new_text"]
                    .as_str()
                    .context("provide replacement text")?;
                change.push(format!("Replace:\n{old}\n\nWith:\n{new}"));
            }
            ("Update Docblock note", change.join("\n\n"), None)
        }
        _ => anyhow::bail!("unsupported Docblock write"),
    };
    Ok(
        json!({"operation":operation,"workspace":workspace,"target":target,"change":change,"expires_in_days":expiry}),
    )
}

fn check_base(args: &Value) -> Result<()> {
    ensure!(
        args["base_version"]
            .as_u64()
            .is_some_and(|version| version > 0)
            && args["base_revision"]
                .as_str()
                .is_some_and(|revision| revision.len() == 64
                    && revision.bytes().all(|b| b.is_ascii_hexdigit())),
        "read the Docblock note for its current version and revision before editing"
    );
    Ok(())
}

fn required<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args[key]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .with_context(|| format!("provide the Docblock {key}"))
}

fn tool_output(response: &Value) -> Result<Value> {
    ensure!(
        response["isError"] != true,
        "Docblock declined the request. Check your workspace access."
    );
    let text = response["content"]
        .as_array()
        .and_then(|content| content.iter().find(|item| item["type"] == "text"))
        .and_then(|item| item["text"].as_str())
        .context("Docblock did not return a tool result")?;
    serde_json::from_str(text).context("Docblock returned an invalid tool result")
}

pub(crate) fn read_result(response: &Value) -> Result<Value> {
    if response["isError"] == true {
        return Ok(
            json!({"docblock_refused":true,"message":"Docblock could not open the requested note. Check the note and workspace scope."}),
        );
    }
    let output = tool_output(response)?;
    let note = &output["note"];
    let body = note["body"]
        .as_str()
        .context("Docblock did not return the note body")?;
    let text = |key: &str, limit| note[key].as_str().map(|value| json_excerpt(value, limit));
    let tags: Vec<_> = note["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .take(20)
        .map(|tag| json_excerpt(tag, 120))
        .collect();
    let mut result = json!({
        "id":text("id", 100),"workspaceId":text("workspaceId", 100),
        "path":text("path", 480),"title":text("title", 320),"tags":tags,
        "version":note["version"].as_u64(),"revisionToken":text("revisionToken", 80),
        "url":docblock_url(&note["url"], false).filter(|url| url.len() <= 1_000),
        "body":"","partial":false
    });
    let body_budget = READ_RESULT_BYTES.saturating_sub(result.to_string().len());
    let excerpt = json_excerpt(clip(body, 6_000), body_budget);
    result["body"] = json!(excerpt);
    result["partial"] = json!(excerpt.len() != body.len());
    Ok(result)
}

fn json_excerpt(value: &str, limit: usize) -> &str {
    let mut bytes = 0;
    for (index, character) in value.char_indices() {
        bytes += match character {
            '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
            '\u{0}'..='\u{1f}' => 6,
            _ => character.len_utf8(),
        };
        if bytes > limit {
            return &value[..index];
        }
    }
    value
}

pub(crate) fn write_result(tool: &str, response: &Value) -> Result<Value> {
    if response["isError"] == true {
        return Ok(
            json!({"docblock_refused":true,"message":"Docblock declined the change. Check scope, workspace access, and the current note version before proposing another change."}),
        );
    }
    let output = tool_output(response)?;
    if tool == "create_share" {
        let share = &output["share"];
        let url = docblock_url(&share["url"], true)
            .context("Docblock did not confirm a valid public link")?;
        ensure!(
            share["kind"] == "note" && share["active"] == true && share["expiresAt"].is_string(),
            "Docblock did not confirm a temporary note link"
        );
        return Ok(json!({"published":true,"public_url":url,"expires_at":share["expiresAt"]}));
    }
    let applied = output["applied"]
        .as_bool()
        .context("Docblock did not confirm the note change")?;
    let note = &output["note"];
    if applied {
        let id = note["id"]
            .as_str()
            .context("Docblock did not return the note id")?;
        Uuid::parse_str(id).context("Docblock returned an invalid note id")?;
        let url = docblock_url(&note["url"], false);
        return Ok(
            json!({"applied":true,"note":{"id":id,"workspaceId":note["workspaceId"],"path":note["path"],"version":note["version"],"url":url}}),
        );
    }
    Ok(
        json!({"applied":false,"proposal_url":docblock_url(&output["proposal"]["url"],false),"message":"The change is awaiting review in Docblock."}),
    )
}

fn docblock_url(value: &Value, share: bool) -> Option<&str> {
    let raw = value.as_str()?;
    let url = net::website(raw).ok()?;
    if url.origin().ascii_serialization() != ORIGIN || url.query().is_some() {
        return None;
    }
    let valid = if share {
        url.fragment().is_none()
            && url.path().strip_prefix("/s/shr_").is_some_and(|token| {
                (10..=64).contains(&token.len())
                    && token
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            })
    } else {
        url.path()
            .strip_prefix('/')
            .is_some_and(|slug| !slug.is_empty() && !slug.contains('/'))
            && url.fragment().is_some_and(|fragment| {
                fragment == "/review"
                    || fragment
                        .strip_prefix("/note/")
                        .is_some_and(|id| Uuid::parse_str(id).is_ok())
            })
    };
    valid.then_some(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        security::{Vault, now},
        store,
    };
    use sqlx::sqlite::SqlitePoolOptions;

    #[test]
    fn binds_replacements_and_defaults_publication_to_one_week() {
        let mut args = json!({"workspace":"notes","kind":"note","path":"demo"});
        prepare_arguments("create_share", &mut args, "task", 3);
        assert_eq!(review("create_share", &args).unwrap()["expires_in_days"], 7);
        args["kind"] = "folder".into();
        assert!(review("create_share", &args).is_err());
        let mut edit = json!({"workspace":"notes","note":{"path":"demo"},"base_version":2,
            "replacements":[{"old_text":"old","new_text":"new"}]});
        assert!(review("update_note", &edit).is_err());
        edit["base_revision"] = "a".repeat(64).into();
        prepare_arguments("update_note", &mut edit, "task", 3);
        let first = edit["request_id"].clone();
        Uuid::parse_str(first.as_str().unwrap()).unwrap();
        prepare_arguments("update_note", &mut edit, "task", 3);
        assert_eq!(edit["request_id"], first);
        prepare_arguments("update_note", &mut edit, "task", 4);
        assert_ne!(edit["request_id"], first);
        assert!(
            review("update_note", &edit).unwrap()["change"]
                .as_str()
                .unwrap()
                .contains("old\n\nWith:\nnew")
        );
    }

    #[test]
    fn distinguishes_private_note_urls_from_public_share_urls() {
        let id = Uuid::new_v4();
        let private = format!("https://docblock.dev/example#/note/{id}");
        let payload = json!({"applied":true,"note":{"id":id.to_string(),"path":"agenda","url":private,"body":"private body"}});
        let response = |data: Value| json!({"content":[{"type":"text","text":data.to_string()}]});
        let result = write_result("write_note", &response(payload)).unwrap();
        assert_eq!(result["note"]["url"], private);
        assert!(result["note"].get("body").is_none());
        let public = "https://docblock.dev/s/shr_synthetic_123";
        let mut payload = json!({"share":{"url":public,"kind":"note","active":true,"expiresAt":"2026-12-01T08:00:00Z"}});
        assert_eq!(
            write_result("create_share", &response(payload.clone())).unwrap()["public_url"],
            public
        );
        payload["share"]["url"] = "https://lookalike.example/s/shr_synthetic_123".into();
        assert!(write_result("create_share", &response(payload)).is_err());
        assert_eq!(
            write_result("create_share", &json!({"isError":true})).unwrap()["docblock_refused"],
            true
        );
    }

    #[test]
    fn docblock_read_result_stays_complete_json_when_the_note_needs_escaping() {
        let id = Uuid::new_v4();
        let body = "\u{b}\u{b}📝💬\n".repeat(1_000);
        let note = json!({
            "note": {"id":id,"path":"daily/2026-11-28","title":"Daily report",
            "tags":["draft"],"version":3,"revisionToken":"a".repeat(64),"body":body,
            "url":format!("https://docblock.dev/example#/note/{id}")}
        });
        let response = json!({"content":[{"type":"text","text":note.to_string()}]});
        let result = read_result(&response).unwrap();
        assert_eq!(result["id"], id.to_string());
        assert_eq!(result["partial"], true);
        let excerpt = result["body"].as_str().unwrap();
        assert!(body.starts_with(excerpt));
        assert!(!excerpt.is_empty());
        assert!(result.to_string().len() <= READ_RESULT_BYTES);
    }

    #[tokio::test]
    async fn scopes_and_share_approval_are_owned_and_one_time() {
        let db = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!().run(&db).await.unwrap();
        store::seed(&db, "1").await.unwrap();
        store::seed(&db, "2").await.unwrap();
        let vault = Vault::new(&"f0".repeat(32)).unwrap();
        let mcp = Mcp::new(
            db.clone(),
            vault.clone(),
            "https://tinybot.example".parse().unwrap(),
        );
        let connector = mcp.prepare_docblock("1").await.unwrap();
        assert_eq!(connector.endpoint, ENDPOINT);
        assert_eq!(mcp.prepare_docblock("1").await.unwrap().id, connector.id);
        assert_ne!(mcp.prepare_docblock("2").await.unwrap().id, connector.id);
        let credential = json!({"flow":{"connector":connector.id,"client_id":"test","client_secret":null,
            "token_url":"https://docblock.dev/oauth/token","verifier":"test","resource":ENDPOINT},
            "scopes":["notes:read"],"access_token":"synthetic","refresh_token":null,"expires":now()+60});
        let sealed = vault
            .seal(&binding(&connector), credential.to_string().as_bytes())
            .unwrap();
        sqlx::query("UPDATE connectors SET auth='oauth',credential=? WHERE id=?")
            .bind(sealed)
            .bind(&connector.id)
            .execute(&db)
            .await
            .unwrap();
        let connector = mcp.owned("1", &connector.id).await.unwrap();
        assert_eq!(mcp.docblock_access(&connector), Some("read"));
        sqlx::query(
            "INSERT INTO tasks(id,user,title,updated) VALUES('docblock-task','1','Fixture',0)",
        )
        .execute(&db)
        .await
        .unwrap();
        let task = store::claim(&db).await.unwrap().unwrap();
        store::pause(
            &db,
            &task,
            "connector-setup",
            &json!({"connector":connector.id}),
            "Fixture",
            None,
            600,
            "sign-in",
        )
        .await
        .unwrap();
        mcp.wake_connected_with_access("1", &connector.id, Some("read"))
            .await
            .unwrap();
        let task = store::claim(&db).await.unwrap().unwrap();
        assert_eq!(task.id, "docblock-task");
        let approval = json!({"call":{"name":"call_mcp","args":{"connector":connector.id,"tool":"create_share",
                "arguments":{"workspace":"notes","kind":"note","path":"demo","expires_in_days":7}}},
            "mcp_approval":{"connector":connector.id,"tool":"create_share","epoch":connector.approval_epoch},"docblock":true});
        store::pause(
            &db, &task, "approve", &approval, "Fixture", None, 600, "review",
        )
        .await
        .unwrap();
        assert!(
            store::respond_mcp(&db, "1", "review", store::McpApprovalDecision::Always)
                .await
                .is_err()
        );
        assert!(
            store::approve_docblock_share(&db, "2", "review", 14)
                .await
                .is_err()
        );
        assert!(
            store::approve_docblock_share(&db, "1", "review", 0)
                .await
                .is_err()
        );
        store::approve_docblock_share(&db, "1", "review", 14)
            .await
            .unwrap();
        let stored: String = sqlx::query_scalar(
            "SELECT payload FROM pending WHERE id='review' AND state='approved'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let stored: Value = serde_json::from_str(&stored).unwrap();
        assert_eq!(stored["call"]["args"]["arguments"]["expires_in_days"], 14);
    }
}
