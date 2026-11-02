//! The `ClusterMailSender` controller: one Stalwart account for an address
//! several namespaces share, and the same Secret holding its app password in each
//! of them.
//!
//! Every copy of the Secret holds the same password, so any one of them tells
//! whether it is still good; when it is not, a new one is written to all of them
//! before the old one is removed from the account.

use std::sync::Arc;

use futures::StreamExt;
use k8s_openapi::api::core::v1::Secret;
use kube::api::{DeleteParams, Patch, PatchParams};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::finalizer::{Event, finalizer};
use kube::runtime::watcher;
use kube::{Api, Resource, ResourceExt};
use serde_json::json;

use crate::config::Address;
use crate::crd::{ClusterMailSender, ClusterMailSenderStatus, Phase, cluster_account_marker};
use crate::reconcile::{
    Context, Error, FINALIZER, MANAGER, RETRY, ROTATE, Stored, build_secret, ensure_account,
    issue_password, owned_by, remove_account,
};

/// Runs the controller until a shutdown signal. It does not watch the Secrets it
/// writes: they are owned by a cluster-scoped resource, which kube-runtime's
/// owner mapping would look up in the Secret's namespace. A deleted copy is
/// rewritten on the next requeue instead.
pub async fn run(context: Arc<Context>) {
    Controller::new(
        Api::<ClusterMailSender>::all(context.kube.clone()),
        watcher::Config::default(),
    )
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

async fn reconcile(object: Arc<ClusterMailSender>, context: Arc<Context>) -> Result<Action, Error> {
    let api = Api::<ClusterMailSender>::all(context.kube.clone());
    finalizer(&api, FINALIZER, object, |event| async {
        match event {
            Event::Apply(object) => apply(&object, &context).await,
            Event::Cleanup(object) => cleanup(&object, &context).await,
        }
    })
    .await
    .map_err(|error| Error::Finalizer(Box::new(error)))
}

fn error_policy(object: Arc<ClusterMailSender>, error: &Error, _: Arc<Context>) -> Action {
    tracing::warn!(object = %object.name_any(), %error, "retrying");
    Action::requeue(RETRY)
}

async fn apply(object: &ClusterMailSender, context: &Context) -> Result<Action, Error> {
    let name = object.name_any();
    let api = Api::<ClusterMailSender>::all(context.kube.clone());
    let previous = object.status.clone().unwrap_or_default();
    let status = |phase, message, address: Option<String>, namespaces: Vec<String>| {
        ClusterMailSenderStatus {
            phase: Some(phase),
            message,
            address,
            secret_name: Some(object.spec.secret_name(&name)),
            namespaces,
            observed_generation: object.metadata.generation,
        }
    };
    // A refused spec keeps what was written until it is fixed.
    let failed = |message: String| {
        status(
            Phase::Failed,
            Some(message),
            previous.address.clone(),
            previous.namespaces.clone(),
        )
    };

    let address = match Address::parse(&object.spec.address) {
        Ok(address) => address,
        Err(message) => {
            let denied = status(
                Phase::Denied,
                Some(message),
                previous.address.clone(),
                previous.namespaces.clone(),
            );
            patch_status(&api, &name, denied).await?;
            return Ok(Action::await_change());
        }
    };

    match sync(object, &address, context).await {
        Ok(()) => {
            let ready = status(
                Phase::Ready,
                None,
                Some(address.to_string()),
                object.spec.namespaces.clone(),
            );
            patch_status(&api, &name, ready).await?;
            Ok(Action::requeue(context.config.requeue()))
        }
        // Held by someone else, often a `MailSender` this sender replaces: checked
        // again at the usual pace, as deleting that sender frees both.
        Err(error @ (Error::AddressTaken(_) | Error::SecretTaken(_))) => {
            patch_status(&api, &name, failed(error.to_string())).await?;
            Ok(Action::requeue(context.config.requeue()))
        }
        Err(error) => {
            patch_status(&api, &name, failed(error.to_string())).await?;
            Err(error)
        }
    }
}

async fn sync(
    object: &ClusterMailSender,
    address: &Address,
    context: &Context,
) -> Result<(), Error> {
    let name = object.name_any();
    let marker = cluster_account_marker(&name);
    let stalwart = &context.stalwart;
    let secret_name = object.spec.secret_name(&name);
    let secrets = |namespace: &str| Api::<Secret>::namespaced(context.kube.clone(), namespace);

    // Checked in every namespace before anything changes, so a Secret someone
    // else holds stops the sender rather than leaving it half written.
    let mut existing = Vec::new();
    for namespace in &object.spec.namespaces {
        if let Some(secret) = secrets(namespace).get_opt(&secret_name).await? {
            if !owned_by(&secret, object) {
                return Err(Error::SecretTaken(format!("{namespace}/{secret_name}")));
            }
            existing.push(secret);
        }
    }

    ensure_account(stalwart, address, &marker).await?;
    let rotate = object.annotations().get(ROTATE).map(String::as_str);
    let stored = Stored::read(existing.first());
    let issued = issue_password(
        stalwart,
        address,
        &stored,
        rotate,
        &format!("ClusterMailSender {name}"),
    )
    .await?;

    for namespace in &object.spec.namespaces {
        let secret = build_secret(
            object.controller_owner_ref(&()),
            namespace,
            &secret_name,
            address,
            &issued,
            &context.config,
        );
        secrets(namespace)
            .patch(
                &secret_name,
                &PatchParams::apply(MANAGER).force(),
                &Patch::Apply(&secret),
            )
            .await?;
    }

    // Only once every Secret holds the new password: a failure before this
    // leaves the old one working, and the next reconcile removes the strays.
    stalwart
        .destroy_app_passwords(&address.to_string(), &issued.stale)
        .await?;

    let previous = object.status.clone().unwrap_or_default();
    for (namespace, stale_name) in stale_copies(&previous, &object.spec.namespaces, &secret_name) {
        if let Some(stale) = secrets(namespace).get_opt(stale_name).await?
            && owned_by(&stale, object)
        {
            secrets(namespace)
                .delete(stale_name, &DeleteParams::default())
                .await?;
        }
    }

    // `address` changed: the old account sends for nobody.
    if let Some(old) = previous.address
        && old != address.to_string()
    {
        remove_account(&old, &marker, stalwart).await?;
    }

    Ok(())
}

/// The copies the last `Ready` reconcile wrote that the spec no longer asks for:
/// each `(namespace, secret name)` in a namespace dropped from the spec, or every
/// one when the Secret was renamed.
fn stale_copies<'a>(
    previous: &'a ClusterMailSenderStatus,
    namespaces: &[String],
    secret_name: &str,
) -> Vec<(&'a str, &'a str)> {
    let Some(previous_name) = previous.secret_name.as_deref() else {
        return Vec::new();
    };
    previous
        .namespaces
        .iter()
        .filter(|namespace| previous_name != secret_name || !namespaces.contains(namespace))
        .map(|namespace| (namespace.as_str(), previous_name))
        .collect()
}

async fn cleanup(object: &ClusterMailSender, context: &Context) -> Result<Action, Error> {
    // The Secrets go with the resource through their owner references.
    let marker = cluster_account_marker(&object.name_any());
    let address = object
        .status
        .as_ref()
        .and_then(|status| status.address.clone())
        .unwrap_or_else(|| object.spec.address.clone());
    remove_account(&address, &marker, &context.stalwart).await?;
    Ok(Action::await_change())
}

async fn patch_status(
    api: &Api<ClusterMailSender>,
    name: &str,
    status: ClusterMailSenderStatus,
) -> Result<(), Error> {
    api.patch_status(
        name,
        &PatchParams::default(),
        &Patch::Merge(json!({ "status": status })),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(namespaces: &[&str], secret_name: &str) -> ClusterMailSenderStatus {
        ClusterMailSenderStatus {
            secret_name: Some(secret_name.to_owned()),
            namespaces: namespaces.iter().map(|ns| (*ns).to_owned()).collect(),
            ..ClusterMailSenderStatus::default()
        }
    }

    fn spec(namespaces: &[&str]) -> Vec<String> {
        namespaces.iter().map(|ns| (*ns).to_owned()).collect()
    }

    #[test]
    fn keeps_the_copies_the_spec_still_asks_for() {
        let previous = written(&["auth", "mentor"], "noreply-smtp");
        let stale = stale_copies(
            &previous,
            &spec(&["auth", "mentor", "shop"]),
            "noreply-smtp",
        );
        assert!(stale.is_empty());
    }

    #[test]
    fn drops_the_copy_of_a_removed_namespace() {
        let previous = written(&["auth", "mentor"], "noreply-smtp");
        let stale = stale_copies(&previous, &spec(&["auth"]), "noreply-smtp");
        assert_eq!(stale, [("mentor", "noreply-smtp")]);
    }

    #[test]
    fn drops_every_copy_under_the_old_name_after_a_rename() {
        let previous = written(&["auth", "mentor"], "noreply-smtp");
        let stale = stale_copies(&previous, &spec(&["auth", "mentor"]), "mail");
        assert_eq!(
            stale,
            [("auth", "noreply-smtp"), ("mentor", "noreply-smtp")]
        );
    }

    #[test]
    fn a_sender_never_ready_has_nothing_to_drop() {
        let never_ready = ClusterMailSenderStatus::default();
        let stale = stale_copies(&never_ready, &spec(&[]), "noreply-smtp");
        assert!(stale.is_empty());
    }
}
