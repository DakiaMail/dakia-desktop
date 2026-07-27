use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Security {
    Tls,
    StartTls,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProviderPreset {
    pub id: &'static str,
    pub name: &'static str,
    pub domains: &'static [&'static str],
    pub imap_host: &'static str,
    pub imap_port: u16,
    pub imap_security: Security,
    pub smtp_host: &'static str,
    pub smtp_port: u16,
    pub smtp_security: Security,
    pub archive_mailbox: &'static str,
    pub spam_mailbox: &'static str,
    pub oauth: bool,
    pub app_password_help: Option<&'static str>,
}

const PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        id: "gmail",
        name: "Gmail",
        domains: &["gmail.com", "googlemail.com"],
        imap_host: "imap.gmail.com",
        imap_port: 993,
        imap_security: Security::Tls,
        smtp_host: "smtp.gmail.com",
        smtp_port: 465,
        smtp_security: Security::Tls,
        archive_mailbox: "[Gmail]/All Mail",
        spam_mailbox: "[Gmail]/Spam",
        // Google must still approve the restricted Gmail scope before this is
        // suitable for broad release, but the configured desktop client can
        // use the standard OAuth + PKCE flow locally.
        oauth: true,
        app_password_help: Some("https://support.google.com/accounts/answer/185833"),
    },
    ProviderPreset {
        id: "outlook",
        name: "Outlook.com",
        domains: &["outlook.com", "hotmail.com", "live.com", "msn.com"],
        imap_host: "outlook.office365.com",
        imap_port: 993,
        imap_security: Security::Tls,
        smtp_host: "smtp-mail.outlook.com",
        smtp_port: 587,
        smtp_security: Security::StartTls,
        archive_mailbox: "Archive",
        spam_mailbox: "Junk",
        // Outlook OAuth is intentionally unavailable until Dakia has a Microsoft Entra
        // application registration. The password path supports Outlook.com app passwords.
        oauth: false,
        app_password_help: Some(
            "https://support.microsoft.com/en-us/office/add-your-outlook-com-account-in-outlook-for-windows-642c1902-bdd7-4dc3-abe7-76d60b148b23",
        ),
    },
    ProviderPreset {
        id: "fastmail",
        name: "Fastmail",
        domains: &["fastmail.com", "fastmail.fm"],
        imap_host: "imap.fastmail.com",
        imap_port: 993,
        imap_security: Security::Tls,
        smtp_host: "smtp.fastmail.com",
        smtp_port: 465,
        smtp_security: Security::Tls,
        archive_mailbox: "Archive",
        spam_mailbox: "Spam",
        oauth: false,
        app_password_help: Some("https://www.fastmail.help/hc/en-us/articles/360058752854"),
    },
    ProviderPreset {
        id: "zoho",
        name: "Zoho Mail",
        domains: &["zoho.com", "zohomail.com"],
        imap_host: "imap.zoho.com",
        imap_port: 993,
        imap_security: Security::Tls,
        smtp_host: "smtp.zoho.com",
        smtp_port: 465,
        smtp_security: Security::Tls,
        archive_mailbox: "Archive",
        spam_mailbox: "Spam",
        oauth: false,
        app_password_help: Some(
            "https://www.zoho.com/mail/help/adminconsole/two-factor-authentication.html#alink6",
        ),
    },
    ProviderPreset {
        id: "migadu",
        name: "Migadu",
        domains: &[],
        imap_host: "imap.migadu.com",
        imap_port: 993,
        imap_security: Security::Tls,
        smtp_host: "smtp.migadu.com",
        smtp_port: 465,
        smtp_security: Security::Tls,
        archive_mailbox: "Archive",
        spam_mailbox: "Junk",
        oauth: false,
        app_password_help: None,
    },
    ProviderPreset {
        id: "icloud",
        name: "iCloud Mail",
        domains: &["icloud.com", "me.com", "mac.com"],
        imap_host: "imap.mail.me.com",
        imap_port: 993,
        imap_security: Security::Tls,
        smtp_host: "smtp.mail.me.com",
        smtp_port: 587,
        smtp_security: Security::StartTls,
        archive_mailbox: "Archive",
        spam_mailbox: "Junk",
        oauth: false,
        app_password_help: Some("https://support.apple.com/102654"),
    },
    ProviderPreset {
        id: "yahoo",
        name: "Yahoo Mail",
        domains: &["yahoo.com", "ymail.com"],
        imap_host: "imap.mail.yahoo.com",
        imap_port: 993,
        imap_security: Security::Tls,
        smtp_host: "smtp.mail.yahoo.com",
        smtp_port: 465,
        smtp_security: Security::Tls,
        archive_mailbox: "Archive",
        spam_mailbox: "Bulk Mail",
        oauth: false,
        app_password_help: Some(
            "https://help.yahoo.com/kb/generate-manage-third-party-passwords-sln15241.html",
        ),
    },
    ProviderPreset {
        id: "custom",
        name: "Other IMAP / SMTP",
        domains: &[],
        imap_host: "",
        imap_port: 993,
        imap_security: Security::Tls,
        smtp_host: "",
        smtp_port: 465,
        smtp_security: Security::Tls,
        archive_mailbox: "Archive",
        spam_mailbox: "Spam",
        oauth: false,
        app_password_help: None,
    },
];

pub fn all() -> &'static [ProviderPreset] {
    PRESETS
}

pub fn by_id(id: &str) -> Option<&'static ProviderPreset> {
    PRESETS.iter().find(|provider| provider.id == id)
}

pub fn detect(email: &str) -> &'static ProviderPreset {
    let domain = email
        .rsplit_once('@')
        .map(|(_, domain)| domain.to_ascii_lowercase());
    domain
        .and_then(|domain| {
            PRESETS
                .iter()
                .find(|preset| preset.domains.contains(&domain.as_str()))
        })
        .unwrap_or_else(|| by_id("custom").expect("custom provider is always present"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_known_provider() {
        assert_eq!(detect("hello@gmail.com").id, "gmail");
        assert_eq!(detect("hello@outlook.com").id, "outlook");
        assert_eq!(detect("hello@example.test").id, "custom");
    }

    #[test]
    fn outlook_uses_the_app_password_preset_until_oauth_is_configured() {
        let outlook = by_id("outlook").expect("outlook preset");
        assert!(!outlook.oauth);
        assert_eq!(outlook.imap_host, "outlook.office365.com");
        assert_eq!(outlook.smtp_host, "smtp-mail.outlook.com");
        assert_eq!(outlook.smtp_security, Security::StartTls);
        assert!(outlook.app_password_help.is_some());
    }

    #[test]
    fn gmail_enables_the_configured_oauth_flow() {
        let gmail = by_id("gmail").expect("gmail preset");
        assert!(gmail.oauth);
        assert_eq!(gmail.imap_host, "imap.gmail.com");
        assert_eq!(gmail.smtp_host, "smtp.gmail.com");
        assert!(gmail.app_password_help.is_some());
    }
}
