use crate::model::{ProviderFailure, provider_failure};

pub(super) struct Failure {
    pub category: &'static str,
    pub message: &'static str,
    pub retryable: bool,
}

pub(super) fn classify(error: &anyhow::Error) -> Failure {
    let (category, message, retryable) = match provider_failure(error) {
        Some(ProviderFailure::Setup) => (
            "model_setup_required",
            "This task needs a model. Open Settings & forms → Model to choose one.",
            false,
        ),
        Some(ProviderFailure::OpenRouterKey) => (
            "openrouter_key_required",
            "OpenRouter needs a working key. Open Settings & forms → Model to check or replace it.",
            false,
        ),
        Some(ProviderFailure::OpenRouterCredits) => (
            "openrouter_credit_limit",
            "OpenRouter reported a credit or spending limit. Check your OpenRouter balance and key limits.",
            false,
        ),
        Some(ProviderFailure::CodexSignIn) => (
            "codex_sign_in_required",
            "The ChatGPT connection needs to be reauthorized. Open Settings & forms → Model to reconnect it.",
            false,
        ),
        Some(ProviderFailure::CodexService) => (
            "codex_service_unavailable",
            "The Codex service on this TinyBot server is unavailable. Choose another connected model or ask the bot operator to check it.",
            false,
        ),
        Some(ProviderFailure::BackupDisabled) => (
            "backup_model_disabled",
            "The backup model is no longer enabled. Check your task and backup models in Settings & forms → Model.",
            false,
        ),
        Some(ProviderFailure::ModelUnavailable) => (
            "model_not_available",
            "The selected model is unavailable for this connection. Choose another model in Settings & forms → Model.",
            false,
        ),
        Some(ProviderFailure::RateLimited) => (
            "model_rate_limited",
            "The model provider reached a rate or usage limit. Give it some time before continuing.",
            true,
        ),
        Some(ProviderFailure::TimedOut) => (
            "model_timed_out",
            "The model provider took too long to respond. Your task is saved.",
            true,
        ),
        Some(ProviderFailure::Temporary) => (
            "model_temporarily_unavailable",
            "The model provider is temporarily unavailable. Your task is saved.",
            true,
        ),
        Some(ProviderFailure::Incomplete) => (
            "model_reply_incomplete",
            "The model didn’t return a usable reply. If it keeps happening, choose another task model in Settings & forms.",
            true,
        ),
        Some(ProviderFailure::InputLimit) => (
            "model_context_limit",
            "The task has more context than the model can process. Use /new with a shorter brief or choose a model with a larger context window.",
            false,
        ),
        Some(ProviderFailure::OutputLimit) => (
            "model_output_limit",
            "The model reached its response limit before it finished. Ask for a shorter result or break the task into smaller steps.",
            false,
        ),
        Some(ProviderFailure::Refused) => (
            "model_refused",
            "The model declined this request. Send a different request to continue.",
            false,
        ),
        Some(ProviderFailure::AccessDenied) => (
            "model_access_denied",
            "The model provider blocked this request. Check the permissions for your connected account in Settings & forms → Model.",
            false,
        ),
        Some(ProviderFailure::Rejected) => (
            "model_request_rejected",
            "The model provider rejected this request. Check that your selected model supports tools, or choose another model in Settings & forms.",
            false,
        ),
        None => (
            "task_failed",
            "I paused this task after an internal error. Open Audit in Settings & forms to see where the run stopped.",
            false,
        ),
    };
    Failure {
        category,
        message,
        retryable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_details_use_a_closed_set_and_hide_raw_errors() {
        let source = "private test provider payload sk-or-fixture";
        for (failure, category, retryable) in [
            (
                ProviderFailure::OpenRouterKey,
                "openrouter_key_required",
                false,
            ),
            (ProviderFailure::RateLimited, "model_rate_limited", true),
            (ProviderFailure::Incomplete, "model_reply_incomplete", true),
            (ProviderFailure::Refused, "model_refused", false),
        ] {
            let classified = classify(&anyhow::Error::new(failure).context(source));
            assert_eq!(classified.category, category);
            assert_eq!(classified.retryable, retryable);
            assert!(!classified.message.contains(source));
        }
        let unknown = classify(&anyhow::anyhow!(source));
        assert_eq!(unknown.category, "task_failed");
        assert!(!unknown.message.contains(source));
        assert!(!unknown.retryable);
    }
}
