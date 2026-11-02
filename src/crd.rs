//! The `MailSender` custom resource: what an app declares to get SMTP credentials.

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// An address an app sends mail from. The operator creates a Stalwart account for
/// it and writes SMTP credentials into a Secret next to it, named by `secretName`.
#[derive(CustomResource, Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "stalwart.foli.ag",
    version = "v1alpha1",
    kind = "MailSender",
    plural = "mailsenders",
    shortname = "msender",
    namespaced,
    status = "MailSenderStatus",
    printcolumn = r#"{"name":"Address","type":"string","jsonPath":".spec.address"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct MailSenderSpec {
    /// The address the app sends from, such as `noreply@example.com`. Its domain
    /// must be one Stalwart serves, and the namespace's policy must allow it.
    pub address: String,
    /// The Secret the credentials are written to. Defaults to `<name>-smtp`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_name: Option<String>,
}

impl MailSenderSpec {
    pub fn secret_name(&self, resource_name: &str) -> String {
        self.secret_name
            .clone()
            .unwrap_or_else(|| format!("{resource_name}-smtp"))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MailSenderStatus {
    pub phase: Option<Phase>,
    /// Why the last reconcile did not reach `Ready`.
    pub message: Option<String>,
    /// The address the Stalwart account was created for, so a changed
    /// `spec.address` can remove the old account.
    pub address: Option<String>,
    /// The Secret holding the credentials.
    pub secret_name: Option<String>,
    pub observed_generation: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum Phase {
    /// The account exists and the Secret holds a working app password.
    Ready,
    /// The namespace's policy refuses the address; nothing was changed.
    Denied,
    /// Talking to Stalwart or writing the Secret failed; retried.
    Failed,
}

/// An address several namespaces send from, such as a platform's shared
/// `noreply@`: one Stalwart account, whose credentials the operator writes into
/// the same Secret in each of `namespaces`. Cluster-scoped, so only whoever may
/// create cluster resources can claim an address this way; the namespace policy,
/// which guards `MailSender`, does not apply.
#[derive(CustomResource, Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "stalwart.foli.ag",
    version = "v1alpha1",
    kind = "ClusterMailSender",
    plural = "clustermailsenders",
    shortname = "cmsender",
    status = "ClusterMailSenderStatus",
    printcolumn = r#"{"name":"Address","type":"string","jsonPath":".spec.address"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct ClusterMailSenderSpec {
    /// The address the apps send from. Its domain must be one Stalwart serves.
    pub address: String,
    /// The namespaces the credentials are written to. Removing one deletes its
    /// Secret.
    pub namespaces: Vec<String>,
    /// The Secret the credentials are written to in each namespace. Defaults to
    /// `<name>-smtp`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_name: Option<String>,
}

impl ClusterMailSenderSpec {
    pub fn secret_name(&self, resource_name: &str) -> String {
        self.secret_name
            .clone()
            .unwrap_or_else(|| format!("{resource_name}-smtp"))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClusterMailSenderStatus {
    pub phase: Option<Phase>,
    /// Why the last reconcile did not reach `Ready`.
    pub message: Option<String>,
    /// The address the Stalwart account was created for, so a changed
    /// `spec.address` can remove the old account.
    pub address: Option<String>,
    /// The Secret holding the credentials.
    pub secret_name: Option<String>,
    /// The namespaces the Secret was written to, so one dropped from the spec
    /// has its Secret deleted.
    #[serde(default)]
    pub namespaces: Vec<String>,
    pub observed_generation: Option<i64>,
}

/// The description the operator gives the accounts it creates. It is how it tells
/// its own accounts from people's, and never touches an account without it.
pub fn account_marker(namespace: &str, name: &str) -> String {
    format!("Managed by stalwart-operator for MailSender {namespace}/{name}")
}

/// [`account_marker`] for a [`ClusterMailSender`]'s account.
pub fn cluster_account_marker(name: &str) -> String {
    format!("Managed by stalwart-operator for ClusterMailSender {name}")
}

/// Both CustomResourceDefinitions, as `deploy/crd.yaml` holds them.
pub fn crds_yaml() -> Result<String, serde_yaml::Error> {
    use kube::CustomResourceExt;
    Ok(format!(
        "{}---\n{}",
        serde_yaml::to_string(&MailSender::crd())?,
        serde_yaml::to_string(&ClusterMailSender::crd())?
    ))
}

#[cfg(test)]
mod tests {
    /// The manifests install `deploy/crd.yaml`; it must be what this code serves.
    /// Regenerate with `cargo run -q -- crd > deploy/crd.yaml`.
    #[test]
    fn committed_crd_matches_the_code() {
        let generated = super::crds_yaml().expect("serialize");
        assert_eq!(
            include_str!("../deploy/crd.yaml"),
            generated,
            "deploy/crd.yaml is stale: run `cargo run -q -- crd > deploy/crd.yaml`"
        );
    }
}
