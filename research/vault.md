# Research: HashiCorp Vault integration surface for Giles (ticket #64)

Researched 2026-10-28 against Vault docs for the current major line (docs tree `v2.x`; latest release [v2.2.1, 2026-10-22](https://github.com/hashicorp/vault/releases/tag/v2.2.1)) and OpenBao [v2.7.2, 2026-10-26](https://github.com/openbao/openbao/releases/tag/v2.7.2). "[INFERENCE]" marks my reasoning, not a source claim. "Unverified" marks something I could not confirm.

## Summary

- **KV v2 is the static-secret store to target.** It's a plain HTTP/JSON API under `/v1/<mount>/…`, with separate path prefixes: `data/` (read/write/patch), `metadata/` (list, version history, `custom_metadata`, `max_versions`, `cas_required`), and `delete/`, `undelete/`, `destroy/` for version lifecycle. It keeps 10 versions by default and supports check-and-set writes. KV secrets have no leases, so nothing needs renewing: Giles reads on demand or caches. `LIST` results are not filtered by policy, so secret names must not carry sensitive data. ([API](https://developer.hashicorp.com/vault/api-docs/secret/kv/kv-v2), [setup/ACL](https://developer.hashicorp.com/vault/docs/secrets/kv/kv-v2/setup))
- **Every auth method ends in a Vault token, and Giles has to keep that token alive.** Service tokens have a TTL. They're renewed with `POST auth/token/renew-self` up to a max TTL (32 days system default), after which Giles must log in again. A *periodic* token, which AppRole can issue via `token_period`, never expires as long as it's renewed within each period. Batch tokens can't be renewed. Revoking a token revokes all of its leases. ([tokens](https://developer.hashicorp.com/vault/docs/concepts/tokens), [token API](https://developer.hashicorp.com/vault/api-docs/auth/token))
- **Which auth method fits depends on where Giles runs.** On a self-hosted host, AppRole (role_id + secret_id; the secret_id can be single-use, CIDR-bound, short-lived, and delivered response-wrapped) or TLS client-cert auth (no bearer secret on disk beyond a private key) fit best. In the container image on Kubernetes, use the Kubernetes auth method (service-account JWT). On clouds, AWS IAM, GCP, Azure MSI or JWT/OIDC remove the "secret zero" entirely. A static token is the simplest option but the weakest. Every method still leaves Giles holding one bootstrap credential, unless the host's platform identity serves as that credential.
- **Vault Agent/Proxy is an alternative to writing auth code.** Agent or Proxy can handle login and renewal (auto-auth) and either write the token to a sink file or inject it into requests made through a local listener (`use_auto_auth_token`). Giles would then talk only to the local proxy, at the cost of running a second daemon. ([auto-auth](https://developer.hashicorp.com/vault/docs/agent-and-proxy/autoauth), [Proxy API proxy](https://developer.hashicorp.com/vault/docs/agent-and-proxy/proxy/apiproxy))
- **The SSH secrets engine has two remaining modes.** Dynamic keys were removed in Vault 1.13.
  - **Signed certificates:** sshd trusts the Vault CA via `TrustedUserCAKeys`. `ssh/sign/<role>` signs a public key Giles supplies; `ssh/issue/<role>` also generates the private key. Certificates are short-lived, scoped by role (`allowed_users`, extensions, TTL), not renewable, and not stored by Vault. Vault doesn't revoke them: they simply expire, or the target lists them in an OpenSSH KRL.
  - **OTP:** needs `vault-ssh-helper` (PAM) on each target, and the target must be able to reach Vault.

  For "the agent SSHes into a server", signed certificates fit best. The private key can stay in a Giles-controlled process and never reach the LLM.
- **Vault policies sit beside Giles's ACLs, not in place of them.** Vault policies are path- and capability-based, deny by default, and attached to the token. If Giles uses one Vault identity, Vault sees only "Giles", and Giles's own ACL must decide which user, agent or plugin may use which secret. Vault then caps the blast radius (for example `giles/data/*` with read only). Finer splits are possible: child tokens whose policies must be a subset of the parent's, per-agent AppRoles, or identity-templated paths. Giles can also check permissions in advance with `sys/capabilities-self`, and can record "on behalf of" in Vault's audit log via token `meta` or audited request headers.
- **OpenBao (Linux Foundation, MPL-2.0) is compatible at the API level only, by policy.** Its compatibility promise covers the secrets- and auth-plugin APIs, not `/sys` and not on-disk storage. The core APIs a provider would use exist in both: KV v2, SSH, AppRole, cert, Kubernetes, JWT and token auth. But OpenBao ships no built-in AWS/GCP/Azure auth or cloud secrets engines. Its tokens look different (`s.`/`b.`/`r.` versus Vault's `hvs.`/`hvb.`/`hvr.`), so Giles must not parse token formats. OpenBao includes namespaces and control groups, which are Enterprise-only in Vault.
- **Licences.** Vault 1.15.0 and later are under BSL 1.1 (licensor: IBM since the 2025 acquisition). The additional use grant allows production use except in a competing paid hosted or embedded offering, and each version converts to MPL 2.0 after four years. Vault's Go `api/` and `sdk/` directories remain MPL-2.0. Giles only talks to Vault over HTTP and doesn't embed or redistribute it, so the BSL doesn't constrain Giles's own licence [INFERENCE; not legal advice]. A Giles provider that targets the shared API would serve both servers.
- **Rust clients.** `vaultrs` 0.8.0 (MIT; ~3M recent downloads; active, last push 2026-10-07) is the only mature async client.
  - **TLS:** its default feature is `rustls`, which maps to `reqwest/rustls` on reqwest 0.13, the same reqwest/rustls stack Giles already uses, so no OpenSSL. `native-tls` is opt-in.
  - **Coverage:** KV v2, SSH (sign/creds/OTP), token renew/lookup, wrapping, and AppRole/K8s/cert/JWT/AWS login.
  - **Gaps:** no `sys/leases` renew/revoke and no `ssh/issue` wrapper. It builds its own reqwest client, adding CAs on top of the default roots, and reads `VAULT_*` env vars by default. It holds the token as a plain `String`.

  The `VaultClient` fields and the rustify `Client::new(base, reqwest::Client)` are public, so Giles can inject its own reqwest client (the one that trusts only `webpki-root-certs`) or implement the `Client` trait [INFERENCE from source]. The alternatives are a brand-new single-maintainer `openbao` crate (MIT/Apache, rustls default, 3 stars) and the abandoned `hashicorp_vault` crate (last release 2022, reqwest 0.11).

---

## 1. KV v2 API

Source: [KV v2 HTTP API](https://developer.hashicorp.com/vault/api-docs/secret/kv/kv-v2) unless noted.

| Operation | Method & path | Notes |
|---|---|---|
| Configure mount | `POST /:mount/config` | `max_versions` (0 ⇒ 10 kept), `cas_required`, `delete_version_after` |
| Read | `GET /:mount/data/:path?version=N` | Response `data.data` = the key/value map; `data.metadata` = `created_time`, `deletion_time`, `destroyed`, `version`, `custom_metadata`. `custom_metadata` is returned even without read access to `metadata/` |
| Create/update | `POST /:mount/data/:path` | Body `{ "options": {"cas": N}, "data": {...} }`; `cas: 0` = create-only; needs `create` (new) or `update` (existing) |
| Patch | `PATCH /:mount/data/:path` | Partial update of latest version; needs `patch` |
| Subkeys | `GET /:mount/subkeys/:path` | Returns key structure with values replaced by null (useful for a UI that shows which fields exist without exposing values) |
| Soft delete latest | `DELETE /:mount/data/:path` | Undo with `undelete` |
| Soft delete versions | `POST /:mount/delete/:path` `{versions:[…]}` | |
| Undelete | `POST /:mount/undelete/:path` | |
| Destroy (permanent) | `PUT /:mount/destroy/:path` | Removes version data |
| List | `LIST /:mount/metadata/:path` | Folders suffixed `/`; **no policy filtering of keys**; needs `list` on `metadata/` even if all other access is via `data/` |
| Read metadata | `GET /:mount/metadata/:path` | All versions, `current_version`, `max_versions`, `cas_required`, `custom_metadata` |
| Write/patch metadata | `POST`/`PATCH /:mount/metadata/:path` | `max_versions`, `cas_required`, `delete_version_after`, `custom_metadata` (string→string map) |
| Delete key entirely | `DELETE /:mount/metadata/:path` | Deletes metadata and all versions |

- The ACL prefixes are `data/` (applies to the latest version), and `undelete/`, `destroy/` and `metadata/` (apply to any version). KV policies don't support `allowed_parameters`, `denied_parameters` or `required_parameters`. ([KV v2 setup](https://developer.hashicorp.com/vault/docs/secrets/kv/kv-v2/setup))
- On non-dev servers, a `kv` mount is v1 unless enabled with `-version=2` or as `kv-v2`. ([setup](https://developer.hashicorp.com/vault/docs/secrets/kv/kv-v2/setup))
- KV issues no leases, although it may return a `lease_duration`. ([lease concepts](https://developer.hashicorp.com/vault/docs/concepts/lease))
- Change notification: KV v2 emits `kv-v2/data-write` and related events over a WebSocket (`/v1/sys/events/subscribe/{type}`), but event notifications are **Vault Enterprise-only**. ([events](https://developer.hashicorp.com/vault/docs/concepts/events)) So a Community Edition (CE) deployment needs polling, or re-reading on use, to pick up rotated values. Whether OpenBao has events is unverified.
- Implications for Giles [INFERENCE]: a `{ secret = "name" }` reference could map to `<mount>/data/<prefix>/<name>`, with a field selector because a KV entry is a map. Version pinning (`?version=`) and CAS give the web UI safe "edit secret" semantics. Because list isn't filtered, secret names must not encode sensitive data.

## 2. Auth methods for a self-hosted daemon and how each is renewed

General token lifecycle ([tokens](https://developer.hashicorp.com/vault/docs/concepts/tokens), [token API](https://developer.hashicorp.com/vault/api-docs/auth/token)):

- **Login result.** Every auth method's login returns `auth.client_token`, `lease_duration` and `renewable`.
- **Renewal.** `POST /auth/token/renew-self` takes an optional `increment`. The effective max TTL is the lowest of: the system max (32 days by default), the mount max, and the value the auth method suggests. It's re-evaluated at each renewal. If the renewed TTL stops growing, the client should log in again.
- **Explicit max TTL** is a hard cap that applies even to periodic tokens.
- **Periodic tokens** reset to their period on each renewal and are the only non-root tokens with unlimited lifetime. They're available via `sudo`/root, token-store roles, or auth methods such as AppRole (`token_period`).
- **Batch tokens** are not renewable, not revocable, and have no accessor. AppRole docs recommend them for machine auth. A batch token's dynamic-secret leases belong to its parent.
- **Revocation and binding.** Revoking a token revokes its child tokens and every lease created with it. Tokens can be CIDR-bound (`token_bound_cidrs`).
- **Implementing renewal.** Vault's own guidance is to inspect the TTL returned by each renewal ([lease](https://developer.hashicorp.com/vault/docs/concepts/lease)). Go clients get `LifetimeWatcher` ([auth concepts](https://developer.hashicorp.com/vault/docs/concepts/auth)); `vaultrs` has no equivalent, so Giles would write its own renew loop [verified by reading vaultrs sources; see §6].

| Method | Credential Giles must hold | Renewal / re-login | Fit for Giles |
|---|---|---|---|
| **Token** ([token auth](https://developer.hashicorp.com/vault/docs/auth/token)) | A Vault token (file/env) | `renew-self` until max TTL; then a human must mint a new one unless periodic | Simplest; a periodic orphan token from a token role is the "set and forget" variant. The token on disk is the bearer secret (disk-theft threat) |
| **AppRole** ([docs](https://developer.hashicorp.com/vault/docs/auth/approle), [API](https://developer.hashicorp.com/vault/api-docs/auth/approle)) | `role_id` (semi-secret) + `secret_id` | Token renewed via `renew-self`; on expiry re-login with a secret_id. secret_ids have their own `secret_id_ttl` and `secret_id_num_uses` and don't renew | Designed for machines. secret_id can be single-use, CIDR-bound (`secret_id_bound_cidrs`), and delivered **response-wrapped** so only Giles ever sees it. A long-lived secret_id on disk is again secret zero |
| **TLS certificate** ([docs](https://developer.hashicorp.com/vault/docs/auth/cert)) | Client cert + private key (`ExtKeyUsage` ClientAuth) | Re-login any time with the same cert; token TTL per role (`ttl`) | No bearer secret is sent; the key could live in a keychain or TPM [INFERENCE]. Vault must terminate TLS itself, or a proxy must forward the validated cert in a header. CRL/OCSP revocation is supported |
| **Kubernetes** ([docs](https://developer.hashicorp.com/vault/docs/auth/kubernetes)) | Pod service-account JWT (projected file) | Re-login with a fresh SA token; Vault validates via TokenReview | Natural for the container image on k8s. Alternatively, JWT auth with k8s as OIDC issuer (no reviewer JWT, but tokens can't be revoked early) |
| **AWS** ([docs](https://developer.hashicorp.com/vault/docs/auth/aws)) | None stored: `iam` signs `sts:GetCallerIdentity` with instance/role creds; `ec2` uses signed instance metadata | Re-login with fresh platform creds | No secret zero on EC2/ECS/Lambda; Giles needs SigV4 signing (vaultrs-login has AWS) |
| **GCP** ([docs](https://developer.hashicorp.com/vault/docs/auth/gcp)) / **Azure** ([docs](https://developer.hashicorp.com/vault/docs/auth/azure)) | None stored: signed JWT from GCP IAM/GCE metadata, or an Azure managed-identity JWT | Re-login | Same idea on GCP/Azure |
| **JWT/OIDC** ([docs](https://developer.hashicorp.com/vault/docs/auth/jwt)) | Any workload JWT (CI, SPIFFE, k8s issuer) | Re-login; since 1.17 `bound_audiences` must exactly match `aud` | Generic workload identity |

Related mechanisms:

- **Response wrapping** ([docs](https://developer.hashicorp.com/vault/docs/concepts/response-wrapping)): Vault puts the response into the cubbyhole of a single-use token with its own short TTL. That gives cover in transit, detection of tampering (if the unwrap fails, someone else already unwrapped it), and limited exposure lifetime. Policies can make wrapping mandatory (`min_wrapping_ttl`). ([policies](https://developer.hashicorp.com/vault/docs/concepts/policies))
- **Vault Agent / Vault Proxy** ([overview](https://developer.hashicorp.com/vault/docs/agent-and-proxy), [auto-auth](https://developer.hashicorp.com/vault/docs/agent-and-proxy/autoauth)): auto-auth logs in with a configured method, renews the token until renewal is denied, logs in again, and writes the token to sinks (optionally wrapped or encrypted). Proxy can inject the auto-auth token into proxied requests (`use_auto_auth_token = true` or `"force"`). ([apiproxy](https://developer.hashicorp.com/vault/docs/agent-and-proxy/proxy/apiproxy)) For Giles this is an operator-side option that moves auth complexity out of Giles, at the cost of a second daemon and a local socket or port that other local processes could also use [INFERENCE].

## 3. Leases, dynamic secrets, and the SSH secrets engine

Leases ([lease concepts](https://developer.hashicorp.com/vault/docs/concepts/lease)):

- Every dynamic secret, and every service token, has a lease: `lease_id`, `lease_duration` and `renewable`.
- Renewal takes an `increment` measured from *now*. The increment is advisory, so the client must read back the TTL actually granted.
- When a lease expires or is revoked, Vault destroys the secret at its source (for example, AWS keys are deleted).
- Revocation can target a prefix (`vault lease revoke -prefix aws/`).
- The endpoints are `sys/leases/renew` and `sys/leases/revoke` [standard API; vaultrs lacks wrappers, see §6].
- Implication [INFERENCE]: a Giles provider for dynamic secrets would need a lease table (lease_id → consumer). It would renew while a plugin or agent session is alive and revoke on session end or Giles shutdown. Because a token's revocation takes its leases with it, revoking a per-session child token cleans everything up at once.

SSH secrets engine ([overview](https://developer.hashicorp.com/vault/docs/secrets/ssh), [API](https://developer.hashicorp.com/vault/api-docs/secret/ssh)):

- **Modes.** Two modes remain: signed certificates and OTP. The dynamic-keys mode was removed in Vault 1.13.
- **Signed certificates** ([docs](https://developer.hashicorp.com/vault/docs/secrets/ssh/signed-ssh-certificates)):
  - **Operator setup:** mount the engine, then `config/ca` (generate or import the CA key; Enterprise can use managed keys). Each target host gets `TrustedUserCAKeys` pointing at the CA public key, which `GET /ssh/public_key` serves unauthenticated.
  - **Role configuration:** a role (`key_type=ca`) fixes `allowed_users` (`*` or a list; templatable with identity, e.g. `ssh-{{identity.entity.id}}-user`), `default_user`, `allowed_extensions`/`default_extensions`, `ttl`/`max_ttl`, and the signing algorithm.
  - **Signing:** `POST /ssh/sign/:role` takes `public_key`, `valid_principals`, `ttl`, `cert_type`, `key_id`, `critical_options` and `extensions`. It returns `signed_key` and `serial_number` with a `lease_id` but `renewable: false`. The certificate is **not stored by Vault**.
  - **Issuing:** `POST /ssh/issue/:role` (Vault 1.12+) generates the key pair *and* the certificate and returns the private key. This means the private key passes through Vault and Giles.
  - **Host-key signing:** a separate mount signs host keys so clients can verify hosts via `@cert-authority` in known_hosts.
  - **Revocation:** no Vault-side revocation of issued certificates appears in the docs. Expiry is the control; early revocation requires an OpenSSH KRL on the target ([`RevokedKeys`](https://man.openbsd.org/sshd_config#RevokedKeys)).
- **One-time passwords** ([docs](https://developer.hashicorp.com/vault/docs/secrets/ssh/one-time-ssh-passwords)):
  - **Flow:** a role (`key_type=otp`, `cidr_list`, `default_user`) issues an OTP via `POST /ssh/creds/:role` with the target `ip`. The OTP comes with a 600 s lease in the example and is not renewable.
  - **Target-side requirement:** each target runs [`vault-ssh-helper`](https://github.com/hashicorp/vault-ssh-helper) (MPL-2.0, Go, invoked via `pam_exec`, maintained, v0.2.4 in 2026-06). The helper calls `ssh/verify` and Vault deletes the OTP.
  - **Logging and weaknesses:** every login is logged in Vault's audit log. Docs list a spoofed target-to-Vault connection as the main risk. The client side typically needs `sshpass` for automation.
- Fit for "the agent SSHes into a server" [INFERENCE]:
  - **Signed certificates need nothing on targets but `TrustedUserCAKeys` and give per-session, minutes-long credentials.** Giles (or the sandboxed SSH tool's launcher) generates an ephemeral key, gets it signed with `valid_principals` limited by Giles's ACL, and hands the key and certificate only to the SSH-executing process. The LLM sees neither.
  - **`ssh/sign` is better than `ssh/issue` here,** because with `sign` the private key never leaves the process that generated it.
  - **OTP suits only fleets where the operator controls PAM on targets.** It also exposes a password-like value that must travel to the SSH client.
- Other dynamic engines, such as databases, cloud credentials and PKI, follow the same lease model ([lease](https://developer.hashicorp.com/vault/docs/concepts/lease)).

## 4. Vault ACL policies beside Giles's ACLs

From [policies](https://developer.hashicorp.com/vault/docs/concepts/policies) unless noted:

- **Policy model.** Policies are HCL/JSON, path-based, and deny by default. Capabilities are `create`, `read`, `update`, `patch`, `delete`, `list`, `sudo`, `deny`, `subscribe` and `recover`. `deny` wins over everything.
- **Path matching.** Paths are matched by the most specific rule: `*` is allowed only as the last character, and `+` matches one path segment. When several policies use the same pattern, their capabilities are unioned.
- **Policy attachment.** Policies attach to tokens: the auth method's role maps the identity to policies at login. A token's policy set is therefore fixed per login [INFERENCE from workflow description].
- **Templated policies.** Paths can interpolate `{{identity.entity.id}}`, `.name`, `.metadata.<k>` and alias metadata, e.g. `secret/data/{{identity.entity.id}}/*`. This needs each principal to be a Vault entity.
- **Parameter constraints and wrapping.** Policies can constrain parameters (`allowed_parameters` and similar; not for KV) and require wrapping via `min_wrapping_ttl`/`max_wrapping_ttl`.
- **Child tokens** ([token API](https://developer.hashicorp.com/vault/api-docs/auth/token)): `policies` on `auth/token/create` must be a **subset** of the caller's policies unless the caller is root or has `sudo`. `meta` on the token is passed through to audit devices. `num_uses` and `ttl` allow single-use or short-lived delegated tokens.
- **Capability checks:** `sys/capabilities-self` returns the calling token's capabilities on given paths. ([API](https://developer.hashicorp.com/vault/api-docs/system/capabilities-self))
- **Audit attribution:** operators can configure request headers to be recorded in audit logs (optionally HMACed) via `sys/config/auditing/request-headers`, which requires `sudo`. ([API](https://developer.hashicorp.com/vault/api-docs/system/config-auditing))
- **Vault Enterprise-only features** that would map onto Giles concepts: namespaces (multi-tenancy) ([docs](https://developer.hashicorp.com/vault/docs/enterprise/namespaces)), control groups (second-person approval before a secret is released) ([docs](https://developer.hashicorp.com/vault/docs/enterprise/control-groups)), and event notifications. OpenBao ships namespaces and control groups in its open-source build ([namespaces](https://openbao.org/docs/concepts/namespaces/), [control groups](https://openbao.org/docs/concepts/control-groups/)).

Layering options for Giles [INFERENCE; the decision belongs to the grilling ticket]:

1. **Giles as a trusted broker, one Vault identity.**
   - The Vault policy grants Giles read (and optionally write) under one prefix, e.g. `giles/data/*` and `giles/metadata/*`.
   - Giles's own ACL decides user, agent and plugin access per secret. Vault only bounds what a fully compromised Giles could reach.
   - Attribution in Vault's audit log comes from token `meta` or audited headers, which Giles supplies and so must be trusted.
2. **Per-agent or per-plugin delegation.**
   - Giles mints short-lived child tokens with a policy subset, or each agent has its own AppRole.
   - Vault then also enforces separation if Giles's ACL has a bug, but the operator must create one Vault policy per scope.
   - Giles would need either pre-made policies or `sys/policy` write access, and that access is itself powerful.
3. **Vault entities per Giles user** with templated policies. This is the heaviest option and mirrors Giles's user model inside Vault, which is only plausible with multi-user plus an identity sync.

Whichever option is chosen, Vault never sees "the LLM". The settled rule that the LLM never sees a value stays a Giles-side guarantee.

## 5. OpenBao: API compatibility and licences

- **OpenBao basics.** OpenBao is a Series of LF Projects, LLC (Linux Foundation) fork, licensed MPL-2.0. ([LICENSE](https://github.com/openbao/openbao/blob/main/LICENSE), [site](https://openbao.org/community/policies/migration/))
- **Compatibility policy** (ratified 2024-02-08): it aims for **API compatibility only** for secrets- and auth-plugin APIs, including external-plugin gRPC, so a client can point at either server. `/sys` APIs are excluded. Seal and storage compatibility are limited, and the project expects to deviate deliberately (pruning plugins and building its own versions of Enterprise features). ([migration policy](https://openbao.org/community/policies/migration/))
- **Migration guide** ([guide](https://openbao.org/docs/guides/migration/)):
  - Tested only for Vault CE 1.14.1 → OpenBao 2.2.0, and makes no storage guarantees for newer Vault.
  - Many plugins aren't shipped; mounts for missing plugins are stubbed.
  - Tokens use the `s.`/`b.`/`r.` form rather than Vault's `hvs.`/`hvb.`/`hvr.`.
  - mlock was removed in 2.0.
  - Vault 2.0+ mounts an `agent-registry/` by default.
- **Built-ins** (OpenBao docs tree, [auth](https://github.com/openbao/openbao/tree/main/website/content/docs/auth), [secrets](https://github.com/openbao/openbao/tree/main/website/content/docs/secrets)):
  - Auth: approle, cert, jwt/oidc, kerberos, kubernetes, ldap, radius, token, userpass. **No aws, gcp or azure auth.** vaultrs notes AWS is "an external plugin" for OpenBao ([README](https://github.com/jmgilman/vaultrs)).
  - Secrets: kv, ssh, pki, transit, databases, kubernetes, ldap, rabbitmq, totp, cubbyhole.
- **Vault licence** ([LICENSE](https://github.com/hashicorp/vault/blob/main/LICENSE)):
  - BSL 1.1 for "Vault Version 1.15.0 or later". The licensor is now IBM ([acquisition closed 2025-02-27](https://newsroom.ibm.com/2025-02-27-ibm-completes-acquisition-of-hashicorp,-creates-comprehensive,-end-to-end-hybrid-cloud-platform)).
  - The additional use grant permits production use except offering Vault to third parties, hosted or embedded, in a paid product competing with IBM's paid versions. Internal use is explicitly not competitive.
  - Change Date: four years after each version is published. Change Licence: MPL 2.0.
  - Announced in [HashiCorp's 2023 BSL post](https://www.hashicorp.com/blog/hashicorp-adopts-business-source-license).
  - The Go client `api/` and `sdk/` directories remain MPL-2.0 ([api/LICENSE](https://github.com/hashicorp/vault/blob/main/api/LICENSE), [sdk/LICENSE](https://github.com/hashicorp/vault/blob/main/sdk/LICENSE)).
- Implication [INFERENCE, not legal advice]: Giles is a network client that neither embeds nor ships Vault, so the BSL doesn't affect Giles's licence. It matters to operators choosing a server, and would matter only if a Giles distribution bundled a Vault binary, e.g. in the container image. A provider limited to KV v2, SSH, token/AppRole/cert/k8s/JWT auth and `renew-self` works against both servers. Cloud-IAM auth would be Vault-only unless the OpenBao operator installs those plugins.

## 6. Rust clients and rustls

| Crate | Status | TLS | Notes |
|---|---|---|---|
| [`vaultrs`](https://crates.io/crates/vaultrs) 0.8.0 (2026-03-17), MIT, [repo](https://github.com/jmgilman/vaultrs) | Maintained (last push 2026-10-07; ~13.6M total, ~3.07M recent downloads; 39 open issues). Tests run against both Vault and OpenBao | Features: `rustls` (**default**) → `rustify/rustls` → `reqwest/rustls`; `rustls-no-provider`; `native-tls`/`native-tls-vendored` opt-in ([Cargo.toml](https://github.com/jmgilman/vaultrs/blob/master/vaultrs/Cargo.toml)). Depends on reqwest 0.13 | Auth: AppRole, AWS, JWT/OIDC, Kubernetes, Token, Cert, Userpass. Secrets: KV1/2, SSH, PKI, Transit, DB, AWS, Cubbyhole; Sys: health, policies, seal, wrapping ([README](https://github.com/jmgilman/vaultrs)). Login helpers in [`vaultrs-login`](https://crates.io/crates/vaultrs-login) |
| [`rustify`](https://crates.io/crates/rustify) 0.7.0 | Same author; the HTTP-endpoint layer under vaultrs | `rustls` default → `reqwest/rustls` | `rustify::clients::reqwest::Client::new(base, reqwest::Client)` is public ([src](https://github.com/jmgilman/rustify/blob/master/src/clients/reqwest.rs)) |
| [`openbao`](https://crates.io/crates/openbao) 2.2.5, MIT OR Apache-2.0 | New (first release 2026-05-27; 40 versions; one contributor; 3 GitHub stars; ~3.2k downloads) | `rustls-tls` default → `reqwest/rustls` (reqwest 0.13); `native-tls` opt-in; `transit-import` pulls `openssl` | Uses `sanitization-secrecy` for secret types. Too young to judge |
| [`hashicorp_vault`](https://crates.io/crates/hashicorp_vault) 2.1.1 | Last release 2022-11 | reqwest 0.11 blocking | Effectively unmaintained |

vaultrs details relevant to Giles (from [`client.rs`](https://github.com/jmgilman/vaultrs/blob/master/vaultrs/src/client.rs)):

- **TLS stack matches Giles's.** Giles's workspace already depends on `reqwest = 0.13.5` with `default-features = false, features = ["http2", "rustls", "stream"]` (repo `Cargo.toml`). With vaultrs's default features, the dependency graph gains no OpenSSL or native-tls [INFERENCE from crate manifests; not test-built].
- **Root store.** `VaultClient::new` builds its own `reqwest::ClientBuilder::new().tls_backend_rustls()`. It *merges* `ca_certs` PEM files into the default roots (`tls_certs_merge`) and supports `danger_accept_invalid_certs` via `verify`. Giles's policy of trusting only bundled `webpki-root-certs`, as in `giles_core::net::tls_client_builder`, therefore isn't expressible through `VaultClientSettings`. It is achievable by building `VaultClient { http: rustify Client::new(addr, giles_reqwest_client), middle, settings }`, since all fields are `pub`, or by implementing vaultrs's `Client` trait [INFERENCE from source].
- **Client certificates:** `identity: Option<reqwest::Identity>` (PEM cert+key) works with rustls. The env-based default identity notes that client certs aren't implemented for native-tls.
- **Environment defaults:** `VAULT_ADDR`, `VAULT_TOKEN`, `VAULT_CACERT`/`VAULT_CAPATH`, `VAULT_SKIP_VERIFY`, `VAULT_CLIENT_CERT`/`VAULT_CLIENT_KEY`. Giles would set every field explicitly so ambient env can't change behaviour [INFERENCE].
- **Token handling:** the token is a plain `String` in settings and middleware (no `zeroize`/`secrecy`).
- **API coverage gaps:** `token::renew_self`/`lookup_self`/`revoke_self` exist, but there is no `sys/leases` renew/revoke and no `ssh/issue` wrapper (`ssh.rs` has creds/OTP, verify and CA `sign`). There is also no background renewal helper. Missing endpoints can be added as rustify `Endpoint`s or raw reqwest calls.
- **Namespaces and wrapping:** `X-Vault-Namespace` is supported (`namespace` setting); response wrapping is supported (`wrap()` / `sys::wrapping::unwrap`).

Alternative [INFERENCE]: the surface Giles needs (login, renew-self, KV v2 read/write/list/metadata, SSH sign, lease renew/revoke) is roughly ten JSON endpoints. A thin in-house client on Giles's existing `reqwest`, with `X-Vault-Token` and `X-Vault-Namespace` headers, is a realistic option compared with depending on vaultrs.

## 7. Other operational facts worth carrying into design

- **Availability.** Vault can be sealed or unavailable; `GET /v1/sys/health` reports init/seal/standby state ([vaultrs sys::health](https://github.com/jmgilman/vaultrs)). Giles needs a policy for "Vault down at startup or reload", e.g. fail closed or keep the last values in memory [INFERENCE].
- **Network egress.** Only the Giles core needs to reach Vault. Plugins and MCP servers keep receiving resolved values (PLAN §7.7) or, for SSH, a key plus certificate, so the sandbox profiles needn't open egress to Vault [INFERENCE from repo docs].
- **Bootstrap credential.** The Vault bootstrap credential (token file, AppRole secret_id, or cert key) replaces `secrets.toml` as the thing to protect on disk. It should keep the same mode-0600 and must-not-be-readable-by-tools rules that PLAN §7.7 and §12 apply to `secrets.toml`, or live in the local provider (OS keychain) [INFERENCE].
