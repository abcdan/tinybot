use super::OpenRouterSettingsError;
use anyhow::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProviderFailure {
    Setup,
    OpenRouterKey,
    OpenRouterCredits,
    CodexSignIn,
    CodexService,
    BackupDisabled,
    ModelUnavailable,
    RateLimited,
    TimedOut,
    Temporary,
    Incomplete,
    InputLimit,
    OutputLimit,
    Refused,
    AccessDenied,
    Rejected,
}

impl ProviderFailure {
    pub(crate) fn category(self) -> &'static str {
        match self {
            Self::Setup => "model_setup_required",
            Self::OpenRouterKey => "openrouter_key_required",
            Self::OpenRouterCredits => "openrouter_credit_limit",
            Self::CodexSignIn => "codex_sign_in_required",
            Self::CodexService => "codex_service_unavailable",
            Self::BackupDisabled => "backup_model_disabled",
            Self::ModelUnavailable => "model_not_available",
            Self::RateLimited => "model_rate_limited",
            Self::TimedOut => "model_timed_out",
            Self::Temporary => "model_temporarily_unavailable",
            Self::Incomplete => "model_reply_incomplete",
            Self::InputLimit => "model_context_limit",
            Self::OutputLimit => "model_output_limit",
            Self::Refused => "model_refused",
            Self::AccessDenied => "model_access_denied",
            Self::Rejected => "model_request_rejected",
        }
    }
}

impl std::fmt::Display for ProviderFailure {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(match self {
            Self::Setup => "Choose a task model in Settings & forms.",
            Self::OpenRouterKey => "The OpenRouter key needs attention in Model settings.",
            Self::OpenRouterCredits => "The OpenRouter account reached a credit or spending limit.",
            Self::CodexSignIn => "Reconnect ChatGPT for Codex in Model settings.",
            Self::CodexService => "The Codex App Server is unavailable.",
            Self::BackupDisabled => "The backup model is no longer enabled.",
            Self::ModelUnavailable => "The selected task model is unavailable.",
            Self::RateLimited => "The model provider reached a rate or usage limit.",
            Self::TimedOut => "The model provider timed out.",
            Self::Temporary => "The model provider is temporarily unavailable.",
            Self::Incomplete => "The model did not return a usable reply.",
            Self::InputLimit => "The context exceeded the task model’s input limit.",
            Self::OutputLimit => "The reply reached the task model’s output limit.",
            Self::Refused => "The task model declined the request.",
            Self::AccessDenied => "The model provider blocked the request.",
            Self::Rejected => "The model provider rejected the request.",
        })
    }
}

impl std::error::Error for ProviderFailure {}

pub(crate) fn provider_failure(error: &Error) -> Option<ProviderFailure> {
    if let Some(failure) = error.downcast_ref::<ProviderFailure>() {
        return Some(*failure);
    }
    if error.is::<tokio::time::error::Elapsed>() {
        return Some(ProviderFailure::TimedOut);
    }
    if let Some(error) = error.downcast_ref::<reqwest::Error>() {
        return http_transport_failure(error);
    }
    error
        .downcast_ref::<OpenRouterSettingsError>()
        .map(|error| match error {
            OpenRouterSettingsError::MissingKey | OpenRouterSettingsError::InvalidKey => {
                ProviderFailure::OpenRouterKey
            }
            OpenRouterSettingsError::InvalidModel | OpenRouterSettingsError::Changed => {
                ProviderFailure::ModelUnavailable
            }
            OpenRouterSettingsError::Unavailable => ProviderFailure::Temporary,
        })
}

pub(super) fn http_transport_failure(error: &reqwest::Error) -> Option<ProviderFailure> {
    if error.is_timeout() {
        Some(ProviderFailure::TimedOut)
    } else if error.is_connect() || error.is_body() {
        Some(ProviderFailure::Temporary)
    } else {
        error
            .status()
            .and_then(|status| transient_status(u64::from(status.as_u16())))
    }
}

pub(super) fn transient_status(code: u64) -> Option<ProviderFailure> {
    match code {
        408 | 504 => Some(ProviderFailure::TimedOut),
        429 => Some(ProviderFailure::RateLimited),
        500..=599 => Some(ProviderFailure::Temporary),
        _ => None,
    }
}

pub(crate) fn temporary_provider_failure(error: &Error) -> bool {
    matches!(
        provider_failure(error),
        Some(ProviderFailure::RateLimited | ProviderFailure::TimedOut | ProviderFailure::Temporary)
    )
}
