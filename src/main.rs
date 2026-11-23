#[cfg(test)]
mod acceptance;
mod audit;
mod channel;
mod forms;
mod mcp;
mod model;
mod net;
mod proxy;
mod runtime;
mod schedules;
mod security;
mod siwc;
mod store;
mod web;
mod workers;

use anyhow::{Context, Result, ensure};
use sqlx::SqlitePool;
use std::{env, net::SocketAddr, path::Path, sync::Arc, time::Duration};

pub struct App {
    db: SqlitePool,
    vault: security::Vault,
    token: String,
    origin: String,
    operator: String,
    models: model::Models,
    mcp: mcp::Mcp,
    forms: forms::Forms,
    workers: workers::Workers,
    tools: Vec<Box<dyn runtime::Tool>>,
}

fn secret(name: &str) -> Result<String> {
    if let Ok(file) = env::var(format!("{name}_FILE")) {
        return Ok(std::fs::read_to_string(file)
            .with_context(|| format!("read {name}_FILE"))?
            .trim()
            .to_string());
    }
    Ok(env::var(name)
        .with_context(|| format!("set {name} or {name}_FILE"))?
        .trim()
        .to_string())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.as_slice() {
        [cmd, action, origin] if cmd == "codex" && action == "connect" => {
            return siwc::connect(origin).await;
        }
        [cmd] if cmd == "generate-key" => {
            println!("{}", security::hash(&security::token()));
            return Ok(());
        }
        [cmd] if cmd == "egress" => return proxy::serve().await,
        [cmd] if matches!(cmd.as_str(), "help" | "--help" | "-h") => {
            println!(
                "tinybot [serve | health | backup PATH | generate-key | codex connect HTTPS_ORIGIN]"
            );
            return Ok(());
        }
        [cmd] if cmd == "health" => {
            reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(3))
                .build()?
                .get("http://127.0.0.1:8080/health")
                .send()
                .await?
                .error_for_status()?;
            return Ok(());
        }
        [cmd, path] if cmd == "backup" => {
            let db = store::open(Path::new(&db_path())).await?;
            sqlx::query("VACUUM INTO ?").bind(path).execute(&db).await?;
            println!("SQLite snapshot written.");
            return Ok(());
        }
        [] => {}
        [cmd] if cmd == "serve" => {}
        _ => anyhow::bail!("Use tinybot --help"),
    }
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();
    let origin = net::https(&env::var("TINYBOT_ORIGIN").context("set TINYBOT_ORIGIN")?)?;
    ensure!(
        origin.path() == "/" && origin.query().is_none(),
        "TINYBOT_ORIGIN must contain only an HTTPS origin"
    );
    let origin_string = origin.origin().ascii_serialization();
    let token = secret("TELEGRAM_BOT_TOKEN")?;
    ensure!(
        token.len() >= 15
            && token.len() <= 150
            && token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-:".contains(&b)),
        "invalid Telegram bot token"
    );
    let operator = env::var("TELEGRAM_OPERATOR_ID").context("set TELEGRAM_OPERATOR_ID")?;
    ensure!(
        operator
            .parse::<u64>()
            .is_ok_and(|id| id > 0 && id <= i64::MAX as u64)
            && !operator.starts_with('+')
            && !operator.starts_with('0'),
        "TELEGRAM_OPERATOR_ID must be the numeric Telegram user ID"
    );
    let vault = security::Vault::new(&secret("TINYBOT_ENCRYPTION_KEY")?)?;
    let db = store::open(Path::new(&db_path())).await?;
    store::seed(&db, &operator).await?;
    store::recover(&db).await?;
    let review_model = env::var("TINYBOT_REVIEW_MODEL")
        .ok()
        .filter(|m| !m.is_empty());
    let review_key = secret("TINYBOT_REVIEW_OPENROUTER_KEY")
        .ok()
        .filter(|m| !m.is_empty());
    let app = Arc::new(App {
        db: db.clone(),
        vault: vault.clone(),
        token: token.clone(),
        origin: origin_string.clone(),
        operator: operator.clone(),
        models: model::Models::new(
            db.clone(),
            vault.clone(),
            env::var("TINYBOT_CODEX_BIN").unwrap_or_else(|_| "codex".into()),
            review_model,
            review_key,
        ),
        mcp: mcp::Mcp::new(db.clone(), vault, origin),
        forms: forms::Forms::default(),
        workers: workers::Workers {
            browser: env::var("TINYBOT_BROWSER_SOCKET")
                .unwrap_or_else(|_| "/browser/browser.sock".into()),
            python: env::var("TINYBOT_PYTHON_SOCKET")
                .unwrap_or_else(|_| "/sandbox/sandbox.sock".into()),
        },
        tools: runtime::tools(),
    });
    let telegram = Arc::new(channel::Telegram::new(token, origin_string)?);
    tokio::spawn(channel::listen(app.clone(), operator, telegram.clone()));
    tokio::spawn(channel::outbox(db.clone(), telegram.clone()));
    tokio::spawn(runtime::run(app.clone(), telegram));
    tokio::spawn(schedules::run(db));
    let address: SocketAddr = env::var("TINYBOT_LISTEN")
        .unwrap_or_else(|_| "0.0.0.0:8080".into())
        .parse()?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    tracing::info!(%address,"TinyBot is ready");
    axum::serve(listener, web::router(app))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

fn db_path() -> String {
    env::var("TINYBOT_DB").unwrap_or_else(|_| "/data/tinybot.sqlite".into())
}
