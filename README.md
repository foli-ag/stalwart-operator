# stalwart-operator

A Kubernetes operator that gives apps SMTP credentials from a [Stalwart](https://stalw.art) mail server. An app declares the address it sends from:

```yaml
apiVersion: stalwart.foli.ag/v1alpha1
kind: MailSender
metadata:
  name: noreply
  namespace: my-app
spec:
  address: noreply@example.com
```

and the operator creates a Stalwart account for that address, gives it an app password, and writes a Secret `noreply-smtp` in the same namespace:

| Key | Example |
|---|---|
| `host` | `mail.example.com` |
| `port` | `465` |
| `tls` | `implicit` or `starttls` |
| `username` | `noreply@example.com` |
| `password` | the app password |
| `address` | `noreply@example.com` |
| `url` | `smtps://noreply%40example.com:…@mail.example.com:465` |
| `credential-id` | the app password's id in Stalwart |

Mail sent with these credentials is signed with the domain's DKIM key like any other mail from the server. Deleting the `MailSender` removes the account and the Secret.

Requires Stalwart 1.0 or later (the JMAP management API).

## How it works

- Accounts are created through Stalwart's management API as an administrator. The operator marks each one with a description naming its `MailSender`, and never touches an account without that mark, so it can't take over a person's mailbox even if a namespace is allowed their address.
- App passwords belong to an account. The operator creates them by logging in as the account with Stalwart's master-user syntax, `<address>%<admin>` and the administrator's password.
- Stalwart generates app passwords and shows them once, so the Secret is the only copy. It records the credential's id: while that credential exists the Secret is kept; if the Secret or the credential is lost, a new password is created, written, and every other app password on the account removed.
- App passwords are checked by Stalwart itself, so they work even when people sign in through an external directory such as OIDC.
- Change the `stalwart.foli.ag/rotate` annotation on a `MailSender` to any new value to rotate its password.
- Changing `spec.address` moves the sender to a new account and removes the old one.

## Install

```sh
kubectl apply -f deploy/crd.yaml -f deploy/operator.yaml
kubectl -n stalwart-operator create secret generic stalwart-operator-admin \
  --from-literal=STALWART_USER=admin --from-literal=STALWART_PASSWORD='…'
kubectl -n stalwart-operator edit configmap stalwart-operator
```

The administrator must be able to manage accounts and to impersonate them with the master-user syntax. Stalwart's recovery administrator (`STALWART_RECOVERY_ADMIN`) can do both.

## Configuration

`operator.toml`, mounted from the ConfigMap:

```toml
# Stalwart's HTTP listener as the operator reaches it, usually its Service.
[stalwart]
url = "http://stalwart.mail.svc:8080"

# What apps connect to, written to each Secret.
[smtp]
host = "mail.example.com"
port = 465            # default
implicit_tls = true   # default; false for STARTTLS on 587

# How often a Ready sender is checked again, in seconds.
requeue_secs = 300

# Which addresses a namespace may claim: exact, or any at a domain.
[namespaces.my-app]
addresses = ["noreply@example.com"]

[namespaces.shop]
addresses = ["*@shop.example.com"]

# For namespaces not listed above. Without it, they may claim nothing.
[default]
addresses = []
```

A `MailSender` the policy refuses gets the `Denied` phase and a message in its status, and nothing is changed in Stalwart. Local parts may only contain letters, digits, `.`, `_` and `-`.

## Development

The flake provides the toolchain:

```sh
nix develop
cargo test
cargo run -q -- crd > deploy/crd.yaml   # after changing src/crd.rs
nix build .#image                        # the container image
```

An ignored test runs the operator's calls against a real Stalwart, creating and removing a test account:

```sh
STALWART_URL=http://localhost:8080 STALWART_USER=admin STALWART_PASSWORD=… \
STALWART_TEST_ADDRESS=operator-test@example.com \
cargo test -- --ignored against_a_real_stalwart
```

## License

Apache-2.0
