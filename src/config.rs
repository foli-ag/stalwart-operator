//! The operator's settings and the per-namespace policy that decides which
//! addresses a `MailSender` may claim.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use url::Url;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub stalwart: StalwartConfig,
    pub smtp: SmtpConfig,
    /// How often a Ready sender is reconciled again, to undo drift.
    #[serde(default = "default_requeue_secs")]
    pub requeue_secs: u64,
    /// What each namespace may claim, overriding `default`.
    #[serde(default)]
    pub namespaces: BTreeMap<String, NamespacePolicy>,
    /// What a namespace not listed in `namespaces` may claim. Absent, such a
    /// namespace may claim nothing.
    #[serde(default)]
    pub default: Option<NamespacePolicy>,
}

#[derive(Debug, Deserialize)]
pub struct StalwartConfig {
    /// Stalwart's HTTP listener, as the operator reaches it: usually its in-cluster
    /// Service, such as `http://stalwart.mail.svc:8080`. The admin credentials come
    /// from `STALWART_USER` and `STALWART_PASSWORD`.
    pub url: Url,
}

#[derive(Debug, Deserialize)]
pub struct SmtpConfig {
    /// The submission host apps connect to, written to each Secret.
    pub host: String,
    /// 465 for implicit TLS, 587 for STARTTLS.
    #[serde(default = "default_port")]
    pub port: u16,
    /// Whether the port speaks TLS from the start (465) rather than STARTTLS (587).
    #[serde(default = "default_true")]
    pub implicit_tls: bool,
}

fn default_requeue_secs() -> u64 {
    300
}

fn default_port() -> u16 {
    465
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Default, Deserialize)]
pub struct NamespacePolicy {
    /// Addresses senders here may claim: an exact address (`noreply@example.com`)
    /// or any address at a domain (`*@example.com`).
    #[serde(default)]
    pub addresses: Vec<String>,
}

/// A checked address: lowercase, one `@`, and a local part made only of letters,
/// digits, `.`, `_` and `-`, so it can't smuggle a sub-address or a master-user
/// separator into Stalwart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Address {
    local: String,
    domain: String,
}

impl Address {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let lower = raw.trim().to_ascii_lowercase();
        let (local, domain) = lower
            .split_once('@')
            .ok_or_else(|| format!("{raw}: not an email address"))?;
        let local_ok = !local.is_empty()
            && !local.starts_with('.')
            && !local.ends_with('.')
            && local
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
        if !local_ok {
            return Err(format!(
                "{raw}: the local part may only hold letters, digits, '.', '_' and '-'"
            ));
        }
        let domain_ok = domain.contains('.')
            && domain.split('.').all(|label| {
                !label.is_empty()
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            });
        if !domain_ok {
            return Err(format!("{raw}: invalid domain"));
        }
        Ok(Self {
            local: local.to_owned(),
            domain: domain.to_owned(),
        })
    }

    pub fn local(&self) -> &str {
        &self.local
    }

    pub fn domain(&self) -> &str {
        &self.domain
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.local, self.domain)
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("reading {}: {error}", path.display()))?;
        Ok(toml::from_str(&text)?)
    }

    pub fn requeue(&self) -> Duration {
        Duration::from_secs(self.requeue_secs)
    }

    /// Checks `address` against its namespace's policy. The message is shown to the
    /// app's owner in the resource's status.
    pub fn admit(&self, namespace: &str, address: &str) -> Result<Address, String> {
        let policy = self
            .namespaces
            .get(namespace)
            .or(self.default.as_ref())
            .ok_or_else(|| format!("namespace {namespace} may not claim mail addresses"))?;
        let address = Address::parse(address)?;
        if policy
            .addresses
            .iter()
            .any(|pattern| address_matches(pattern, &address))
        {
            Ok(address)
        } else {
            Err(format!(
                "namespace {namespace} may not claim {address}; allowed: {}",
                policy.addresses.join(", ")
            ))
        }
    }

    /// The `smtp://` or `smtps://` URL many SMTP libraries take as one setting,
    /// with the credentials percent-encoded.
    pub fn smtp_url(&self, username: &str, password: &str) -> String {
        let scheme = if self.smtp.implicit_tls {
            "smtps"
        } else {
            "smtp"
        };
        let mut url = Url::parse(&format!("{scheme}://{}:{}", self.smtp.host, self.smtp.port))
            .expect("the SMTP host forms a URL");
        // Both only fail for URLs without a host, which this one has.
        let _ = url.set_username(username);
        let _ = url.set_password(Some(password));
        url.to_string()
    }
}

fn address_matches(pattern: &str, address: &Address) -> bool {
    let pattern = pattern.trim().to_ascii_lowercase();
    match pattern.strip_prefix("*@") {
        Some(domain) => address.domain() == domain,
        None => address.to_string() == pattern,
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn config() -> Config {
        toml::from_str(
            r#"
            [stalwart]
            url = "http://stalwart.mail.svc:8080"

            [smtp]
            host = "mail.example.com"

            [namespaces.auth]
            addresses = ["noreply@example.com"]

            [namespaces.apps]
            addresses = ["*@apps.example.com"]
            "#,
        )
        .expect("the example config parses")
    }

    #[rstest]
    #[case::exact("auth", "noreply@example.com", true)]
    #[case::exact_is_case_insensitive("auth", "NoReply@Example.com", true)]
    #[case::exact_only("auth", "billing@example.com", false)]
    #[case::domain_wildcard("apps", "shop@apps.example.com", true)]
    #[case::wildcard_is_not_a_suffix("apps", "shop@evil.apps.example.com", false)]
    #[case::wildcard_other_domain("apps", "shop@example.com", false)]
    #[case::unlisted_namespace("default", "noreply@example.com", false)]
    fn admits_addresses(#[case] namespace: &str, #[case] address: &str, #[case] admitted: bool) {
        assert_eq!(config().admit(namespace, address).is_ok(), admitted);
    }

    #[test]
    fn unlisted_namespaces_fall_back_to_the_default() {
        let config = Config {
            default: Some(NamespacePolicy {
                addresses: vec!["*@example.com".into()],
            }),
            ..config()
        };
        assert!(config.admit("default", "app@example.com").is_ok());
        assert!(config.admit("auth", "app@example.com").is_err());
    }

    #[rstest]
    #[case::plain("noreply@example.com", true)]
    #[case::dots_and_dashes("no-reply.app_1@mail.example.com", true)]
    #[case::sub_address("noreply+tag@example.com", false)]
    #[case::master_user_separator("noreply%admin@example.com", false)]
    #[case::two_ats("a@b@example.com", false)]
    #[case::no_local_part("@example.com", false)]
    #[case::leading_dot(".noreply@example.com", false)]
    #[case::bare_domain("noreply@localhost", false)]
    #[case::empty_label("noreply@example..com", false)]
    fn parses_addresses(#[case] raw: &str, #[case] valid: bool) {
        assert_eq!(Address::parse(raw).is_ok(), valid);
    }

    #[test]
    fn smtp_url_encodes_the_credentials() {
        assert_eq!(
            config().smtp_url("noreply@example.com", "a/b:c"),
            "smtps://noreply%40example.com:a%2Fb%3Ac@mail.example.com:465"
        );
    }

    #[test]
    fn smtp_url_uses_smtp_for_starttls() {
        let mut config = config();
        config.smtp.port = 587;
        config.smtp.implicit_tls = false;
        assert!(config.smtp_url("a@example.com", "p").starts_with("smtp://"));
    }
}
