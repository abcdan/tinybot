use super::{super::ApiError, text};
use crate::{App, proxy};
use anyhow::{Result, ensure};
use axum::http::StatusCode;
use serde_json::Value;

#[derive(Debug)]
pub(super) enum SettingsError {
    Forbidden,
    InvalidUrl,
    Unavailable,
}

impl SettingsError {
    pub(super) fn response(&self) -> ApiError {
        match self {
            Self::Forbidden => ApiError(
                StatusCode::FORBIDDEN,
                "Only the TinyBot operator can change browser proxy settings.",
            ),
            Self::InvalidUrl => ApiError(
                StatusCode::BAD_REQUEST,
                "Enter an HTTP or SOCKS5 proxy URL with a public address. Include a port for SOCKS5.",
            ),
            Self::Unavailable => ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "TinyBot couldn’t save the browser setting. Check that the browser proxy storage is mounted.",
            ),
        }
    }
}

impl std::fmt::Display for SettingsError {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output.write_str(self.response().1)
    }
}

impl std::error::Error for SettingsError {}

fn require_operator(app: &App, user: &str) -> Result<()> {
    ensure!(user == app.operator, SettingsError::Forbidden);
    Ok(())
}

pub(super) fn save(app: &App, user: &str, data: &Value) -> Result<()> {
    require_operator(app, user)?;
    let url = text(data, "url", 2048).map_err(|_| SettingsError::InvalidUrl)?;
    proxy::validate_browser_proxy(url).map_err(|_| SettingsError::InvalidUrl)?;
    proxy::set_browser_proxy(Some(url)).map_err(|_| SettingsError::Unavailable)?;
    Ok(())
}

pub(super) fn reset(app: &App, user: &str) -> Result<()> {
    require_operator(app, user)?;
    proxy::clear_browser_proxy().map_err(|_| SettingsError::Unavailable)?;
    Ok(())
}

pub(super) fn direct(app: &App, user: &str) -> Result<()> {
    require_operator(app, user)?;
    proxy::set_browser_proxy(None).map_err(|_| SettingsError::Unavailable)?;
    Ok(())
}
