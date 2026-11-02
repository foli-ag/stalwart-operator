//! The slice of Stalwart's JMAP management API the operator needs: domains,
//! accounts and app passwords.
//!
//! Accounts are managed as the admin. App passwords belong to an account and are
//! created by logging in as that account through Stalwart's master-user syntax,
//! `<address>%<admin>` with the admin's password.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use url::Url;

const USING: [&str; 2] = ["urn:ietf:params:jmap:core", "urn:stalwart:jmap"];
const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum StalwartError {
    #[error("request to Stalwart failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Stalwart answered HTTP {status}: {body}")]
    Status { status: u16, body: String },
    #[error("Stalwart refused {method}: {kind}: {description}")]
    Method {
        method: String,
        kind: String,
        description: String,
    },
    #[error("Stalwart answered {method} unexpectedly: {detail}")]
    Unexpected { method: String, detail: String },
}

pub struct Stalwart {
    http: reqwest::Client,
    base: Url,
    user: String,
    password: String,
}

/// An account as far as the operator cares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub id: String,
    pub description: Option<String>,
}

/// A freshly created app password. `secret` is only ever shown once.
pub struct AppPassword {
    pub id: String,
    pub secret: String,
}

#[derive(Deserialize)]
struct Session {
    #[serde(rename = "apiUrl")]
    api_url: String,
}

impl Stalwart {
    pub fn new(base: Url, user: String, password: String) -> Result<Self, StalwartError> {
        let http = reqwest::Client::builder().timeout(TIMEOUT).build()?;
        Ok(Self {
            http,
            base,
            user,
            password,
        })
    }

    pub async fn domain_id(&self, name: &str) -> Result<Option<String>, StalwartError> {
        let list = self
            .get(&self.user, "x:Domain/get", json!(["name"]))
            .await?;
        Ok(list
            .iter()
            .find(|domain| domain["name"].as_str() == Some(name))
            .and_then(|domain| domain["id"].as_str().map(str::to_owned)))
    }

    pub async fn find_account(
        &self,
        local: &str,
        domain_id: &str,
    ) -> Result<Option<Account>, StalwartError> {
        let list = self
            .get(
                &self.user,
                "x:Account/get",
                json!(["@type", "name", "domainId", "description"]),
            )
            .await?;
        Ok(list
            .iter()
            .find(|account| {
                account["name"].as_str() == Some(local)
                    && account["domainId"].as_str() == Some(domain_id)
            })
            .and_then(|account| {
                Some(Account {
                    id: account["id"].as_str()?.to_owned(),
                    description: account["description"].as_str().map(str::to_owned),
                })
            }))
    }

    /// Creates a user account with no password: it can only log in with the app
    /// passwords the operator gives it.
    pub async fn create_account(
        &self,
        local: &str,
        domain_id: &str,
        description: &str,
    ) -> Result<String, StalwartError> {
        let created = self
            .create(
                &self.user,
                "x:Account/set",
                json!({
                    "@type": "User",
                    "name": local,
                    "domainId": domain_id,
                    "description": description,
                }),
            )
            .await?;
        id_of("x:Account/set", &created)
    }

    pub async fn destroy_account(&self, id: &str) -> Result<(), StalwartError> {
        self.destroy(&self.user, "x:Account/set", &[id.to_owned()])
            .await
    }

    pub async fn app_password_ids(&self, address: &str) -> Result<Vec<String>, StalwartError> {
        let list = self
            .get(
                &self.as_account(address),
                "x:AppPassword/get",
                json!(["id"]),
            )
            .await?;
        Ok(list
            .iter()
            .filter_map(|credential| credential["id"].as_str().map(str::to_owned))
            .collect())
    }

    pub async fn create_app_password(
        &self,
        address: &str,
        description: &str,
    ) -> Result<AppPassword, StalwartError> {
        let method = "x:AppPassword/set";
        let created = self
            .create(
                &self.as_account(address),
                method,
                json!({ "description": description }),
            )
            .await?;
        let secret = created["secret"]
            .as_str()
            .ok_or_else(|| StalwartError::Unexpected {
                method: method.into(),
                detail: "no secret in the created app password".into(),
            })?;
        Ok(AppPassword {
            id: id_of(method, &created)?,
            secret: secret.to_owned(),
        })
    }

    pub async fn destroy_app_passwords(
        &self,
        address: &str,
        ids: &[String],
    ) -> Result<(), StalwartError> {
        if ids.is_empty() {
            return Ok(());
        }
        self.destroy(&self.as_account(address), "x:AppPassword/set", ids)
            .await
    }

    /// The master-user login that acts as `address`.
    fn as_account(&self, address: &str) -> String {
        format!("{address}%{}", self.user)
    }

    async fn get(
        &self,
        login: &str,
        method: &str,
        properties: Value,
    ) -> Result<Vec<Value>, StalwartError> {
        let response = self
            .call(
                login,
                method,
                json!({ "ids": null, "properties": properties }),
            )
            .await?;
        match response.get("list") {
            Some(Value::Array(list)) => Ok(list.clone()),
            _ => Err(StalwartError::Unexpected {
                method: method.into(),
                detail: "no list in the response".into(),
            }),
        }
    }

    async fn create(
        &self,
        login: &str,
        method: &str,
        value: Value,
    ) -> Result<Value, StalwartError> {
        let response = self
            .call(login, method, json!({ "create": { "new": value } }))
            .await?;
        if let Some(error) = response.pointer("/notCreated/new") {
            return Err(set_error(method, error));
        }
        response
            .pointer("/created/new")
            .cloned()
            .ok_or_else(|| StalwartError::Unexpected {
                method: method.into(),
                detail: "neither created nor notCreated".into(),
            })
    }

    async fn destroy(
        &self,
        login: &str,
        method: &str,
        ids: &[String],
    ) -> Result<(), StalwartError> {
        let response = self.call(login, method, json!({ "destroy": ids })).await?;
        if let Some(Value::Object(errors)) = response.get("notDestroyed")
            && let Some((id, error)) = errors
                .iter()
                // Already gone is what destroying asked for.
                .find(|(_, error)| error["type"].as_str() != Some("notFound"))
        {
            return Err(set_error(&format!("{method} destroy {id}"), error));
        }
        Ok(())
    }

    async fn call(&self, login: &str, method: &str, args: Value) -> Result<Value, StalwartError> {
        let session: Session = self
            .send(self.http.get(self.url("/jmap/session")), login)
            .await?
            .json()
            .await?;
        let body = json!({
            "using": USING,
            "methodCalls": [[method, args, "c0"]],
        });
        let response: Value = self
            .send(
                self.http
                    .post(self.url(&api_path(&session.api_url)))
                    .json(&body),
                login,
            )
            .await?
            .json()
            .await?;
        let (name, result) = response
            .pointer("/methodResponses/0")
            .and_then(|call| Some((call.get(0)?.as_str()?, call.get(1)?)))
            .ok_or_else(|| StalwartError::Unexpected {
                method: method.into(),
                detail: "no method response".into(),
            })?;
        if name == "error" {
            return Err(StalwartError::Method {
                method: method.into(),
                kind: result["type"].as_str().unwrap_or("unknown").into(),
                description: result["description"].as_str().unwrap_or("").into(),
            });
        }
        Ok(result.clone())
    }

    async fn send(
        &self,
        request: reqwest::RequestBuilder,
        login: &str,
    ) -> Result<reqwest::Response, StalwartError> {
        let response = request
            .basic_auth(login, Some(&self.password))
            .send()
            .await?;
        let status = response.status();
        if status.is_success() {
            Ok(response)
        } else {
            let body = response.text().await.unwrap_or_default();
            Err(StalwartError::Status {
                status: status.as_u16(),
                body: body.chars().take(300).collect(),
            })
        }
    }

    fn url(&self, path: &str) -> Url {
        let mut url = self.base.clone();
        url.set_path(path);
        url
    }
}

/// The session's `apiUrl` carries Stalwart's public URL; only its path is used, so
/// the operator keeps talking to the address it was configured with.
fn api_path(api_url: &str) -> String {
    Url::parse(api_url)
        .map(|url| url.path().to_owned())
        .unwrap_or_else(|_| api_url.to_owned())
}

fn id_of(method: &str, created: &Value) -> Result<String, StalwartError> {
    created["id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| StalwartError::Unexpected {
            method: method.into(),
            detail: "no id in the created object".into(),
        })
}

fn set_error(method: &str, error: &Value) -> StalwartError {
    StalwartError::Method {
        method: method.into(),
        kind: error["type"].as_str().unwrap_or("unknown").into(),
        description: error["description"].as_str().unwrap_or("").into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_path_drops_the_public_host() {
        assert_eq!(api_path("https://mail.example.com/jmap/"), "/jmap/");
        assert_eq!(api_path("/jmap/"), "/jmap/");
    }

    /// Runs the operator's calls against a real Stalwart: creates an account and an
    /// app password, checks the password authenticates, then removes both. Run with
    ///
    ///   STALWART_URL=http://localhost:8080 STALWART_USER=admin \
    ///   STALWART_PASSWORD=… STALWART_TEST_ADDRESS=operator-test@example.com \
    ///   cargo test -- --ignored against_a_real_stalwart
    #[tokio::test]
    #[ignore = "needs a running Stalwart"]
    async fn against_a_real_stalwart() {
        let env = |key: &str| std::env::var(key).unwrap_or_else(|_| panic!("{key} is unset"));
        let base = Url::parse(&env("STALWART_URL")).expect("STALWART_URL");
        let stalwart = Stalwart::new(base.clone(), env("STALWART_USER"), env("STALWART_PASSWORD"))
            .expect("client");
        let address = env("STALWART_TEST_ADDRESS");
        let (local, domain) = address.split_once('@').expect("an address");
        let marker = "stalwart-operator integration test";

        let domain_id = stalwart
            .domain_id(domain)
            .await
            .expect("domains")
            .expect("Stalwart serves the test address's domain");
        assert!(
            stalwart
                .find_account(local, &domain_id)
                .await
                .expect("accounts")
                .is_none(),
            "{address} already exists; pick an unused test address"
        );
        let account_id = stalwart
            .create_account(local, &domain_id, marker)
            .await
            .expect("create account");

        let result = async {
            let found = stalwart.find_account(local, &domain_id).await?;
            assert_eq!(found.and_then(|a| a.description).as_deref(), Some(marker));

            let created = stalwart
                .create_app_password(&address, "integration test")
                .await?;
            assert!(created.secret.starts_with("app"));
            assert_eq!(
                stalwart.app_password_ids(&address).await?,
                vec![created.id.clone()]
            );

            // The app password logs in as the account, which is what SMTP AUTH does.
            let session = reqwest::Client::new()
                .get(base.join("/jmap/session").expect("url"))
                .basic_auth(&address, Some(&created.secret))
                .send()
                .await?;
            assert!(
                session.status().is_success(),
                "app password refused: {}",
                session.status()
            );

            stalwart
                .destroy_app_passwords(&address, &[created.id])
                .await?;
            assert!(stalwart.app_password_ids(&address).await?.is_empty());
            Ok::<_, StalwartError>(())
        }
        .await;

        stalwart
            .destroy_account(&account_id)
            .await
            .expect("destroy account");
        assert!(
            stalwart
                .find_account(local, &domain_id)
                .await
                .expect("accounts")
                .is_none()
        );
        result.expect("operator calls");
    }
}
