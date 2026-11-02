//! The controller: keeps a Stalwart account for every `MailSender` and its Secret
//! holding a working app password.
//!
//! Stalwart generates app passwords itself and shows them once, so the Secret is
//! the only copy. It records the credential's id: while that credential exists on
//! the account the Secret is kept, and when either is lost a new app password is
//! created, written to the Secret, and every other one on the account removed.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::ByteString;
use k8s_openapi::api::core::v1::Secret;
use kube::api::{DeleteParams, ObjectMeta, Patch, PatchParams};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::finalizer::{self, Event, finalizer};
use kube::runtime::watcher;
use kube::{Api, Resource, ResourceExt};
use serde_json::json;

use crate::config::{Address, Config};
use crate::crd::{MailSender, MailSenderStatus, Phase, account_marker};
use crate::stalwart::{Stalwart, StalwartError};

const FINALIZER: &str = "stalwart.foli.ag/account";
const MANAGER: &str = "stalwart-operator";
const MANAGED_BY: (&str, &str) = ("app.kubernetes.io/managed-by", "stalwart-operator");
/// Change this annotation's value on a `MailSender` to rotate its app password.
const ROTATE: &str = "stalwart.foli.ag/rotate";
/// The `ROTATE` value the Secret's password was created for.
const ROTATED: &str = "stalwart.foli.ag/rotated";
const RETRY: Duration = Duration::from_secs(30);

const PASSWORD: &str = "password";
const USERNAME: &str = "username";
const CREDENTIAL_ID: &str = "credential-id";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("kubernetes: {0}")]
    Kube(#[from] kube::Error),
    #[error(transparent)]
    Stalwart(#[from] StalwartError),
    #[error("Stalwart serves no domain {0}")]
    UnknownDomain(String),
    #[error("{0} is an existing account not created for this MailSender")]
    AddressTaken(String),
    #[error("Secret {0} exists and is not owned by this MailSender")]
    SecretTaken(String),
    #[error("the resource has no namespace")]
    NoNamespace,
    #[error("finalizer: {0}")]
    Finalizer(#[source] Box<finalizer::Error<Error>>),
}

struct Context {
    kube: kube::Client,
    config: Config,
    stalwart: Stalwart,
}

pub async fn run(kube: kube::Client, config: Config, stalwart: Stalwart) {
    let context = Arc::new(Context {
        kube: kube.clone(),
        config,
        stalwart,
    });
    let owned = watcher::Config::default().labels(&format!("{}={}", MANAGED_BY.0, MANAGED_BY.1));

    Controller::new(
        Api::<MailSender>::all(kube.clone()),
        watcher::Config::default(),
    )
    .owns(Api::<Secret>::all(kube), owned)
    .shutdown_on_signal()
    .run(reconcile, error_policy, context)
    .for_each(|result| async move {
        match result {
            Ok((object, _)) => tracing::debug!(%object, "reconciled"),
            Err(error) => tracing::warn!(%error, "reconcile failed"),
        }
    })
    .await;
}

async fn reconcile(object: Arc<MailSender>, context: Arc<Context>) -> Result<Action, Error> {
    let namespace = object.namespace().ok_or(Error::NoNamespace)?;
    let api = Api::<MailSender>::namespaced(context.kube.clone(), &namespace);

    finalizer(&api, FINALIZER, object, |event| async {
        match event {
            Event::Apply(object) => apply(&object, &namespace, &context).await,
            Event::Cleanup(object) => cleanup(&object, &namespace, &context).await,
        }
    })
    .await
    .map_err(|error| Error::Finalizer(Box::new(error)))
}

fn error_policy(object: Arc<MailSender>, error: &Error, _: Arc<Context>) -> Action {
    tracing::warn!(object = %object.name_any(), %error, "retrying");
    Action::requeue(RETRY)
}

async fn apply(object: &MailSender, namespace: &str, context: &Context) -> Result<Action, Error> {
    let name = object.name_any();
    let api = Api::<MailSender>::namespaced(context.kube.clone(), namespace);
    let status = |phase, message, address: Option<String>| MailSenderStatus {
        phase: Some(phase),
        message,
        address,
        secret_name: Some(object.spec.secret_name(&name)),
        observed_generation: object.metadata.generation,
    };
    let previous = object
        .status
        .as_ref()
        .and_then(|status| status.address.clone());

    // A refused spec changes nothing: the account already created, if any, keeps
    // working until the spec is fixed or the resource deleted.
    let address = match context.config.admit(namespace, &object.spec.address) {
        Ok(address) => address,
        Err(message) => {
            patch_status(&api, &name, status(Phase::Denied, Some(message), previous)).await?;
            return Ok(Action::await_change());
        }
    };

    match sync(object, namespace, &address, context).await {
        Ok(()) => {
            let ready = status(Phase::Ready, None, Some(address.to_string()));
            patch_status(&api, &name, ready).await?;
            Ok(Action::requeue(context.config.requeue()))
        }
        // Retrying cannot free an address someone else holds; the spec has to change.
        Err(error @ (Error::AddressTaken(_) | Error::SecretTaken(_))) => {
            patch_status(
                &api,
                &name,
                status(Phase::Failed, Some(error.to_string()), previous),
            )
            .await?;
            Ok(Action::await_change())
        }
        Err(error) => {
            patch_status(
                &api,
                &name,
                status(Phase::Failed, Some(error.to_string()), previous),
            )
            .await?;
            Err(error)
        }
    }
}

async fn sync(
    object: &MailSender,
    namespace: &str,
    address: &Address,
    context: &Context,
) -> Result<(), Error> {
    let name = object.name_any();
    let marker = account_marker(namespace, &name);
    let stalwart = &context.stalwart;
    let secret_name = object.spec.secret_name(&name);
    let secrets = Api::<Secret>::namespaced(context.kube.clone(), namespace);

    let existing = secrets.get_opt(&secret_name).await?;
    if let Some(secret) = &existing
        && !owned_by(secret, object)
    {
        return Err(Error::SecretTaken(secret_name));
    }

    let domain_id = stalwart
        .domain_id(address.domain())
        .await?
        .ok_or_else(|| Error::UnknownDomain(address.domain().to_owned()))?;
    match stalwart.find_account(address.local(), &domain_id).await? {
        Some(account) if account.description.as_deref() == Some(marker.as_str()) => {}
        Some(_) => return Err(Error::AddressTaken(address.to_string())),
        None => {
            stalwart
                .create_account(address.local(), &domain_id, &marker)
                .await?;
        }
    }

    let login = address.to_string();
    let credentials = stalwart.app_password_ids(&login).await?;
    let rotate = object.annotations().get(ROTATE).map(String::as_str);
    let stored = Stored::read(existing.as_ref());
    let (password, credential_id) = match stored.keep(&login, &credentials, rotate) {
        Some(kept) => kept,
        None => {
            let created = stalwart
                .create_app_password(&login, &format!("MailSender {namespace}/{name}"))
                .await?;
            (created.secret, created.id)
        }
    };

    let rotated = rotate.map(str::to_owned).or(stored.rotated);
    let secret = build_secret(
        object,
        &secret_name,
        address,
        &password,
        &credential_id,
        rotated.as_deref(),
        &context.config,
    );
    secrets
        .patch(
            &secret_name,
            &PatchParams::apply(MANAGER).force(),
            &Patch::Apply(&secret),
        )
        .await?;

    // Only once the Secret holds the new password: a failure before this leaves
    // the old one working, and the next reconcile removes the strays.
    let stale: Vec<String> = credentials
        .into_iter()
        .filter(|id| *id != credential_id)
        .collect();
    stalwart.destroy_app_passwords(&login, &stale).await?;

    // `address` changed: the old account sends for nobody.
    if let Some(old) = object
        .status
        .as_ref()
        .and_then(|status| status.address.as_deref())
        && old != login
    {
        remove_account(old, &marker, stalwart).await?;
    }

    // `secretName` changed: the old Secret holds credentials nothing reads.
    if let Some(old) = object
        .status
        .as_ref()
        .and_then(|status| status.secret_name.as_ref())
        && *old != secret_name
        && let Some(stale) = secrets.get_opt(old).await?
        && owned_by(&stale, object)
    {
        secrets.delete(old, &DeleteParams::default()).await?;
    }

    Ok(())
}

async fn cleanup(object: &MailSender, namespace: &str, context: &Context) -> Result<Action, Error> {
    // The Secret goes with the resource through its owner reference.
    let marker = account_marker(namespace, &object.name_any());
    let address = object
        .status
        .as_ref()
        .and_then(|status| status.address.clone())
        .unwrap_or_else(|| object.spec.address.clone());
    remove_account(&address, &marker, &context.stalwart).await?;
    Ok(Action::await_change())
}

/// Removes the account for `address` if this sender created it.
async fn remove_account(address: &str, marker: &str, stalwart: &Stalwart) -> Result<(), Error> {
    let Ok(address) = Address::parse(address) else {
        return Ok(());
    };
    let Some(domain_id) = stalwart.domain_id(address.domain()).await? else {
        return Ok(());
    };
    if let Some(account) = stalwart.find_account(address.local(), &domain_id).await?
        && account.description.as_deref() == Some(marker)
    {
        stalwart.destroy_account(&account.id).await?;
    }
    Ok(())
}

async fn patch_status(
    api: &Api<MailSender>,
    name: &str,
    status: MailSenderStatus,
) -> Result<(), Error> {
    api.patch_status(
        name,
        &PatchParams::default(),
        &Patch::Merge(json!({ "status": status })),
    )
    .await?;
    Ok(())
}

fn owned_by(secret: &Secret, object: &MailSender) -> bool {
    let uid = object.uid();
    secret
        .owner_references()
        .iter()
        .any(|owner| Some(&owner.uid) == uid.as_ref())
}

/// What an existing Secret holds.
#[derive(Debug, Default, PartialEq)]
struct Stored {
    username: Option<String>,
    password: Option<String>,
    credential_id: Option<String>,
    rotated: Option<String>,
}

impl Stored {
    fn read(secret: Option<&Secret>) -> Self {
        let Some(secret) = secret else {
            return Self::default();
        };
        let value = |key: &str| {
            secret
                .data
                .as_ref()?
                .get(key)
                .and_then(|value| String::from_utf8(value.0.clone()).ok())
                .filter(|value| !value.is_empty())
        };
        Self {
            username: value(USERNAME),
            password: value(PASSWORD),
            credential_id: value(CREDENTIAL_ID),
            rotated: secret.annotations().get(ROTATED).cloned(),
        }
    }

    /// The stored password and its credential id, if they are still good: for the
    /// same address, the credential still on the account, and no rotation asked
    /// since they were created.
    fn keep(
        &self,
        username: &str,
        credentials: &[String],
        rotate: Option<&str>,
    ) -> Option<(String, String)> {
        let rotating = rotate.is_some() && rotate != self.rotated.as_deref();
        let password = self.password.clone()?;
        let credential_id = self.credential_id.clone()?;
        (!rotating
            && self.username.as_deref() == Some(username)
            && credentials.contains(&credential_id))
        .then_some((password, credential_id))
    }
}

fn build_secret(
    object: &MailSender,
    name: &str,
    address: &Address,
    password: &str,
    credential_id: &str,
    rotated: Option<&str>,
    config: &Config,
) -> Secret {
    let username = address.to_string();
    let data = BTreeMap::from([
        ("host".to_owned(), config.smtp.host.clone()),
        ("port".to_owned(), config.smtp.port.to_string()),
        (
            "tls".to_owned(),
            if config.smtp.implicit_tls {
                "implicit"
            } else {
                "starttls"
            }
            .to_owned(),
        ),
        (USERNAME.to_owned(), username.clone()),
        (PASSWORD.to_owned(), password.to_owned()),
        ("address".to_owned(), username.clone()),
        ("url".to_owned(), config.smtp_url(&username, password)),
        (CREDENTIAL_ID.to_owned(), credential_id.to_owned()),
    ]);

    Secret {
        metadata: ObjectMeta {
            name: Some(name.to_owned()),
            namespace: object.namespace(),
            labels: Some(BTreeMap::from([(
                MANAGED_BY.0.to_owned(),
                MANAGED_BY.1.to_owned(),
            )])),
            annotations: rotated
                .map(|rotated| BTreeMap::from([(ROTATED.to_owned(), rotated.to_owned())])),
            owner_references: object.controller_owner_ref(&()).map(|owner| vec![owner]),
            ..ObjectMeta::default()
        },
        type_: Some("Opaque".to_owned()),
        data: Some(
            data.into_iter()
                .map(|(key, value)| (key, ByteString(value.into_bytes())))
                .collect(),
        ),
        ..Secret::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored(rotated: Option<&str>) -> Stored {
        Stored {
            username: Some("noreply@example.com".into()),
            password: Some("app-secret".into()),
            credential_id: Some("c1".into()),
            rotated: rotated.map(str::to_owned),
        }
    }

    fn credentials(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| (*id).to_owned()).collect()
    }

    #[test]
    fn keeps_a_live_credential() {
        let kept = stored(None).keep("noreply@example.com", &credentials(&["c1"]), None);
        assert_eq!(kept, Some(("app-secret".into(), "c1".into())));
    }

    #[test]
    fn replaces_a_credential_removed_from_the_account() {
        let kept = stored(None).keep("noreply@example.com", &credentials(&["c2"]), None);
        assert_eq!(kept, None);
    }

    #[test]
    fn replaces_the_password_when_the_address_changed() {
        let kept = stored(None).keep("billing@example.com", &credentials(&["c1"]), None);
        assert_eq!(kept, None);
    }

    #[test]
    fn rotates_when_the_annotation_moves() {
        let current = credentials(&["c1"]);
        assert!(
            stored(Some("1"))
                .keep("noreply@example.com", &current, Some("1"))
                .is_some()
        );
        assert!(
            stored(Some("1"))
                .keep("noreply@example.com", &current, Some("2"))
                .is_none()
        );
        assert!(
            stored(None)
                .keep("noreply@example.com", &current, Some("1"))
                .is_none()
        );
    }

    #[test]
    fn an_empty_secret_keeps_nothing() {
        assert_eq!(
            Stored::read(None).keep("noreply@example.com", &credentials(&["c1"]), None),
            None
        );
    }
}
