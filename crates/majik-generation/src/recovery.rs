//! Recovery hints for a failed generation, keyed on the structured error kind first and the
//! legacy message text second.

use majik_providers::{ProviderId, ProviderRegistry};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryAction {
    Retry,
    OpenProviderSettings,
    CheckCredits(String),
}

impl RecoveryAction {
    pub fn title(&self) -> &'static str {
        match self {
            RecoveryAction::Retry => "Try Again",
            RecoveryAction::OpenProviderSettings => "Open Provider Settings",
            RecoveryAction::CheckCredits(_) => "Check Credits",
        }
    }

    /// Icon name in the app's bundle (`packaging/icons.json`).
    pub fn icon(&self) -> &'static str {
        match self {
            RecoveryAction::Retry => "refresh-cw",
            RecoveryAction::OpenProviderSettings => "key-round",
            RecoveryAction::CheckCredits(_) => "credit-card",
        }
    }
}

/// `error_kind` is `GenerationError::kind()` when known; `message` is the stored text.
pub fn recovery_action(error_kind: Option<&str>, message: Option<&str>, provider: Option<&ProviderId>) -> RecoveryAction {
    let billing = provider.and_then(|p| ProviderRegistry::shared().descriptor(p)).and_then(|d| d.billing_url);
    let kind = error_kind.map(|k| k.to_string()).unwrap_or_else(|| {
        let lower = message.unwrap_or("").trim().to_lowercase();
        if lower.contains("insufficient credits") || lower.contains("payment required") {
            "payment_required".into()
        } else if lower.contains("authentication failed") || lower.contains("unauthorized") {
            "unauthorized".into()
        } else {
            "other".into()
        }
    });
    match kind.as_str() {
        "payment_required" => match billing {
            Some(url) => RecoveryAction::CheckCredits(url.to_string()),
            None => RecoveryAction::OpenProviderSettings,
        },
        "unauthorized" => RecoveryAction::OpenProviderSettings,
        _ => RecoveryAction::Retry,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_kinds_and_legacy_text() {
        assert_eq!(recovery_action(None, None, None), RecoveryAction::Retry);
        assert_eq!(recovery_action(Some("unauthorized"), None, None), RecoveryAction::OpenProviderSettings);
        assert_eq!(recovery_action(None, Some("Authentication failed: bad key"), None), RecoveryAction::OpenProviderSettings);
        assert_eq!(recovery_action(Some("payment_required"), None, None), RecoveryAction::OpenProviderSettings);
        assert_eq!(recovery_action(Some("timeout"), None, None), RecoveryAction::Retry);
    }
}
