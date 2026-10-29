# Ways to let a process use a credential without exposing its value (ticket #65)

Research for the Giles secrets-storage wayfinding map. These are facts and trade-offs, not a
decision. Repo facts come from `Docs/technical/config.md` (Secrets), `Docs/technical/sandbox.md`, and
`Docs/PLAN.md` §7.7/§12. I did not read issue #25 because the contract says not to touch GitHub. Its
scope comes from the ticket text: isolated local or remote workspaces, such as an EC2 instance
reached over SSH.

## Summary

- **Four tiers of protection.** The patterns differ in where the plaintext lives:
  - **(A) It never enters the tool's process.** The process gets an oracle or a placeholder instead:
    ssh-agent signing, an auth-injecting proxy, sentinel substitution, or MCP URL-mode/OAuth held by
    the client.
  - **(B) The tool's process has it, but the LLM does not.** Examples: environment injection at
    launch (`op run`, Vault Agent, systemd credentials) and references resolved before launch.
  - **(C) Leaks are allowed but bounded.** Examples: short-lived, narrowly scoped credentials such
    as SSH certificates and 1-hour GitHub App tokens.
  - **(D) Output scrubbing**, a best-effort net over the other three.

  Only tier A survives a tool that runs `env`, `cat ~/.ssh/id_*`, or a hostile MCP server. Giles's
  stdio-toolset `inherit_env` path is tier B today.
- **ssh-agent** never hands out the key ([ssh-agent(1)](https://man.openbsd.org/ssh-agent.1)).
  Anyone who can reach the socket can still *use* the loaded keys, and through forwarding this
  includes root on the remote host ([ssh_config(5) ForwardAgent](https://man.openbsd.org/ssh_config)).
  - `ssh-add -c` confirmation goes through `ssh-askpass`, a GUI prompt, and is phishable.
  - `-h` destination constraints (OpenSSH ≥ 8.9) need support on both ends and cover only SSH user
    authentication. They can't stop an attacker on a permitted hop from using the key toward
    permitted destinations ([ssh-add(1)](https://man.openbsd.org/ssh-add),
    [agent-restrict](https://www.openssh.com/agent-restrict.html)).
- **SSH certificates** bound what a leaked key can do: a validity window, principals,
  `force-command`, `source-address`, and no agent forwarding
  ([ssh-keygen(1)](https://man.openbsd.org/ssh-keygen)). The planned Vault provider gets this from
  the Vault SSH engine, whose example role uses `ttl = 30m`
  ([Vault](https://developer.hashicorp.com/vault/docs/secrets/ssh/signed-ssh-certificates)). They
  fit Giles→workspace SSH, where Giles holds the key outside the workspace.
- **Auth-injecting egress proxies are the converging pattern for agents.**
  - Instances: [Envoy credential injector](https://www.envoyproxy.io/docs/envoy/latest/configuration/http/http_filters/credential_injector_filter),
    [Fly tokenizer](https://github.com/superfly/tokenizer),
    [Vault Proxy `use_auto_auth_token = "force"`](https://developer.hashicorp.com/vault/docs/agent-and-proxy/proxy/apiproxy),
    the [Claude Code on the web git proxy](https://www.anthropic.com/engineering/claude-code-sandboxing),
    [Deno Sandbox secrets](https://docs.deno.com/sandbox/security),
    the [OpenClaw secret egress proxy](https://docs.openclaw.ai/gateway/secrets/secret-store-and-egress),
    and [Claude Code sandbox `mask`](https://code.claude.com/docs/en/sandboxing).
  - How it works: the process holds a placeholder, and the proxy swaps in the real value only for
    bound hosts.
  - What it costs: TLS termination with a CA the client must trust, a client that honours the
    proxy, and HTTP only (SigV4 needs re-signing).
  - Remaining leaks: an allowed host that reflects the credential back, and DNS spoofing of an
    allowed name.
- **Launch-time environment injection** (`op run`, Vault Agent process supervisor, systemd
  `LoadCredential=`) keeps secrets out of config and off disk. The child still holds plaintext, so
  `env` or `/proc/self/environ` from that process (or its children) leaks it.
  - `op run` masks exact values in its child's stdout/stderr by default
    ([1Password](https://developer.1password.com/docs/cli/reference/commands/run/)).
  - systemd credentials are files, not inherited environment variables, kept in non-swappable
    memory and optionally encrypted with a TPM2 ([systemd](https://systemd.io/CREDENTIALS/)).
  - Codex cloud's variant removes secrets before the agent phase starts
    ([OpenAI](https://learn.chatgpt.com/docs/environments/cloud-environment)).
- **Secret references** (`op://…`, n8n `{{ $secrets.… }}`, OpenClaw SecretRef, Giles's
  `{ secret = … }`) keep plaintext out of config and model-visible text. They don't protect the
  process the reference is resolved for. OpenClaw says outright that SecretRefs "are not a
  process-isolation boundary", and that tool results and transcripts aren't masked
  ([OpenClaw](https://docs.openclaw.ai/gateway/secrets/runtime-model)).
- **Agent products keep credentials out of the model in two main ways.**
  - Binding: the credential is attached to the tool by an operator, and the model fills in only
    the parameters (n8n's `$fromAI`, the MCP client holding OAuth tokens).
  - Out-of-band entry: MCP **forbids** asking for secrets in form-mode elicitation and requires URL
    mode instead, "so credentials never pass through the LLM context"
    ([MCP 2026-07-28](https://modelcontextprotocol.io/specification/2026-07-28/client/elicitation)).
    OpenClaw's masked `secrets` prompt does the same.
  - MCP says stdio servers should "retrieve credentials from the environment"
    ([authorization](https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization)),
    which makes stdio servers tier B by design.
- **Scrubbing output is exact-match and fails open.** GitHub warns that "automatic redaction is not
  guaranteed": transformed values (Base64, URL encoding), derived values (JWTs), and structured
  secrets (JSON) slip through ([GitHub](https://docs.github.com/en/actions/reference/security/secure-use)).
  - Partial prints, values split across chunks or words, values below a minimum length (Giles skips
    values under 4 bytes), and anything the model re-encodes all evade it.
  - Giles's `scrub` covers logs and stderr today. The docs say nothing about scrubbing tool results
    on their way to the model.
- **Fit for Giles.**
  - **Stdio MCP servers.** Today they get tier B through `inherit_env`. Tier A would need a
    core-hosted injecting proxy reached through a `unix` grant with `network = false`, because
    Giles's network grants filter by port, not host. That also requires Landlock ABI 9 for
    per-socket rules on Linux, plus CA and proxy variables that the `GILES_*`/`LANG` allowlist
    doesn't pass today.
  - **Remote workspaces (#25).** Giles keeps its own key and uses short-lived certificates. For
    credentials needed inside the workspace, the options are:
    - a proxy exposed over `ssh -R remote_socket:local_socket`;
    - secrets limited to the setup phase;
    - short-lived scoped tokens;
    - agent forwarding, the riskiest.
  - **EC2 instance-profile credentials** are readable by any process on the instance through IMDS
    ([AWS](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/instance-metadata-security-credentials.html)),
    so they count as given to the agent.

---

## 0. Framing: the four tiers

| Tier | Where plaintext lives | Examples | Survives a tool that echoes its env/files? |
| --- | --- | --- | --- |
| A. Oracle / placeholder | Only in a component outside the tool's process (agent, proxy, MCP client) | ssh-agent, credential-injecting proxy, sentinels, MCP OAuth held by the client, URL-mode elicitation | Yes for the value. The *capability* can still be abused while the oracle is reachable |
| B. Process holds it, model doesn't | Tool process memory, environment, or credential file | `op run`, Vault Agent `exec`, systemd credentials, Giles `inherit_env`, MCP stdio "from the environment" | No: `env`, `/proc/self/environ`, a credentials file, a debug dump |
| C. Bounded value | Anywhere, but short-lived and narrowly scoped | SSH certificates, GitHub App installation tokens (1 h), Vault dynamic secrets | Leaks, with a limited blast radius |
| D. Scrubbing | Output text after the fact | `op run` masking, GitHub `::add-mask::`, Giles `scrub` | Only exact, untransformed, complete matches |

The threats in scope include a compromised plugin or MCP server and a prompt-injected agent. Only
tier A keeps the value from them. Tiers B to D reduce *accidental* exposure: logs, the config file,
the model's context when the tool behaves.

## 1. ssh-agent and agent forwarding

- **The key never leaves the agent.** "Authentication passphrases and private keys never go over
  the network: the connection to the agent is forwarded over SSH remote connections and the result
  is returned to the requester" ([ssh-agent(1)](https://man.openbsd.org/ssh-agent.1)). The agent
  speaks a small protocol over a Unix socket: list public keys, add or delete keys, sign
  ([agent-restrict](https://www.openssh.com/agent-restrict.html)).
- **Socket access is the whole capability.** `SSH_AUTH_SOCK` "is accessible only to the current
  user, but is easily abused by root or another instance of the same user"
  ([ssh-agent(1)](https://man.openbsd.org/ssh-agent.1)).
  - Forwarding: "Users with the ability to bypass file permissions on the remote host … can access
    the local agent through the forwarded connection. An attacker cannot obtain key material from
    the agent, however they can perform operations on the keys that enable them to authenticate
    using the identities loaded into the agent"
    ([ssh_config(5) ForwardAgent](https://man.openbsd.org/ssh_config)).
  - OpenSSH's own advice is that "it is generally better for users to avoid the use of a forwarded
    agent altogether (e.g. using the ProxyJump directive)"
    ([agent-restrict](https://www.openssh.com/agent-restrict.html)).
- **`ssh-add -c`** marks identities as needing confirmation, which
  [ssh-askpass(1)](https://man.openbsd.org/ssh-add) performs (a zero exit status confirms).
  OpenSSH notes that confirmation "is somewhat easy to phish" and that the popup "can offer no
  information on the destination host … or the forwarding path"
  ([agent-restrict](https://www.openssh.com/agent-restrict.html)). For an unattended daemon such as
  Giles there is no GUI to show an askpass prompt. [INFERENCE] Giles would need its own agent to
  route confirmations to the owner. The Rust crate
  [`ssh-agent-lib`](https://docs.rs/ssh-agent-lib/latest/ssh_agent_lib/) (0.6.0, "types for writing
  custom SSH agents") makes that feasible.
- **`ssh-add -t life`** sets a maximum key lifetime ([ssh-add(1)](https://man.openbsd.org/ssh-add)).
- **Destination constraints (`ssh-add -h`, OpenSSH 8.9+).**
  - What they do: `[user@]host` or `src>[user@]dst` hops, identified by host keys from
    `known_hosts`. "The whole connection path, including ssh-agent forwarding, is tested against
    those constraints" ([ssh-add(1)](https://man.openbsd.org/ssh-add)).
  - Limit: they need support in "both the remote SSH client and server".
  - Limit: they "can only be enforced by ssh-agent when a key is used, or when it is forwarded by a
    cooperating ssh. … it does not prevent an attacker with access to a remote `SSH_AUTH_SOCK` from
    forwarding it again and using it on a different host (but only to a permitted destination)"
    ([ssh-add(1)](https://man.openbsd.org/ssh-add)).
  - Limit: "Restrictions only work for user authentication". Destination-restricted keys can't be
    used for `ssh-keygen` signatures or CA operations
    ([agent-restrict](https://www.openssh.com/agent-restrict.html)).
  - OpenSSH's framing: treat each constraint as "a delegation of a key to a host, that is only
    slightly more trustworthy than the delegate is."
- **What leaks if a tool echoes.**
  - `cat ~/.ssh/id_*` finds nothing if the key exists only in the agent; keep key files out of the
    workspace.
  - `env` shows `SSH_AUTH_SOCK`, a path and not a secret, but any process that can connect to it
    can sign. `ssh-add -L` prints public keys only.
  - So a tool output leaks the *ability* to authenticate while the socket is reachable, never the
    key value.
- **Scrubbing** is irrelevant to the key, which never appears. It can't stop a prompt-injected
  agent from *using* the socket.

## 2. SSH certificates from a CA (short-lived)

- **Certificate restrictions** ([ssh-keygen(1) CERTIFICATES](https://man.openbsd.org/ssh-keygen)):
  - `-V` sets the validity interval;
  - critical options include `force-command=command` and `source-address=address_list`;
  - extensions such as `permit-agent-forwarding` are opt-in, and `no-agent-forwarding` removes it.

  A leaked private key plus certificate is useful only inside that window and those restrictions.
- **Vault SSH secrets engine.** It signs client public keys with a CA that sshd trusts through
  `TrustedUserCAKeys`. Roles set `allowed_users`, `allowed_extensions`, `default_extensions`, and
  `ttl` (the doc example uses `"ttl": "30m0s"`), and requests can pin `valid_principals`. Before
  Vault 1.9, an empty `allowed_extensions` meant "permissive defaults"
  ([Vault](https://developer.hashicorp.com/vault/docs/secrets/ssh/signed-ssh-certificates)).
- **Related: EC2 Instance Connect.** It pushes a *public* key to instance metadata, where it
  "remains for 60 seconds". An established session isn't cut when the key or the IAM credentials
  expire ([AWS](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/ec2-instance-connect-methods.html)).
- **What leaks if a tool echoes.** If the ephemeral private key sits in a file the tool can read,
  `cat` leaks it, bounded by the certificate's validity, principals, and source address. If the key
  is held only by the Giles core (in process or in Giles's own agent) and never placed in the
  workspace, nothing in the workspace holds it.
- **Fit for #25** [INFERENCE]: Giles→workspace connections can use a per-session key pair kept in
  the core, signed by the local provider's CA or by Vault's SSH engine. Settings: a minutes-long
  TTL, principal = workspace user, `source-address` = Giles's egress, and no agent forwarding unless
  the operator chooses it.

## 3. HTTP proxy that adds the auth header (egress-time injection)

The pattern: the tool sends requests through a proxy outside its trust boundary, and the proxy
attaches the credential (or swaps a placeholder for it) only for allowed destinations.

- **[Envoy credential injector](https://www.envoyproxy.io/docs/envoy/latest/configuration/http/http_filters/credential_injector_filter).**
  Injects a Basic or Bearer `Authorization` header, or fetches an OAuth2 client-credentials token.
  Intended for workload identity ("does not handle end user authentication").
- **[Fly.io tokenizer](https://github.com/superfly/tokenizer).**
  - The client holds a secret *sealed* to the proxy's public key and sends it in `Proxy-Tokenizer`;
    the proxy decrypts it and injects the credential.
  - Processors cover bearer, HMAC, SigV4 re-signing, a JWT/client-credentials exchange that returns
    a *sealed* access token ("the caller never sees any plaintext credential"), and GitHub App
    tokens.
  - The sealed secret binds the allowed client (`bearer_auth` digest, or `fly-src` machine binding)
    and `allowed_hosts`.
  - The client must speak plain HTTP to the proxy, which upgrades to HTTPS upstream.
- **[Vault Proxy API proxy](https://developer.hashicorp.com/vault/docs/agent-and-proxy/proxy/apiproxy).**
  `use_auto_auth_token = "force"` makes the proxy ignore any client token and attach its own
  auto-auth Vault token. Vault-only.
- **[Claude Code on the web](https://www.anthropic.com/engineering/claude-code-sandboxing).**
  "Sensitive credentials (such as git credentials or signing keys) are never inside the sandbox."
  A proxy handles git: the in-sandbox git uses "a custom-built scoped credential". The proxy
  "verifies this credential and the contents of the git interaction (e.g. ensuring it is only
  pushing to the configured branch), then attaches the right authentication token before sending
  the request to GitHub."
- **[Deno Sandbox](https://docs.deno.com/sandbox/security).** Secrets are declared with `hosts`.
  Inside the VM, `echo $ANTHROPIC_API_KEY` prints a placeholder, and the platform substitutes the
  real value only on outbound requests to approved hosts. Combined with `allowNet`, "even if code is
  tricked into calling an unexpected endpoint, credentials are never sent."
- **[Claude Code sandbox `credentials` `mask`](https://code.claude.com/docs/en/sandboxing).**
  - Sandboxed commands see a per-session sentinel; the proxy substitutes the value for
    `injectHosts`.
  - Requirements:
    - `network.tlsTerminate`; without it, masking "fails without exposing anything";
    - SigV4 re-signing for AWS; streaming uploads, presigned URLs, and SigV4A can't be recomputed;
    - `extract`/`decode` for structured values such as `DATABASE_URL` and JWTs;
    - honoured only from user or managed scopes, not repository settings.
  - File masking: on Linux, sandboxed commands read a sentinel copy of the file. On macOS the file
    is blocked outright instead.
- **[OpenClaw secret egress proxy](https://docs.openclaw.ai/gateway/secrets/secret-store-and-egress)**
  (default off).
  - The subprocess environment carries `oc-sent-v2…end` sentinels; a Gateway loopback proxy
    replaces them in URLs, headers, Basic credentials, and streamed bodies.
  - Each secret binds exact HTTPS hosts. Bindings are checked before decryption, and an unknown
    sentinel fails closed.
  - Each process gets its own proxy password and grant, revoked when the process exits.
  - Stated limits:
    - "A bound service that reflects request credentials can still return the plaintext to the
      agent";
    - hostname-based policy is open to DNS compromise;
    - the traffic allowlist "constrains only cooperating clients … A subprocess can ignore those
      variables and open raw sockets";
    - WebSocket frames aren't substituted;
    - other Base64 bodies aren't decoded.
- **What leaks if a tool echoes.**
  - `env` shows sentinels, a proxy URL (with a per-process proxy password in OpenClaw), and CA
    paths, not the value.
  - The value can still leak through:
    - a destination that echoes request headers (httpbin-style endpoints, verbose error bodies);
    - the proxy CA private key, if it is readable by the tool;
    - a proxy that injects for every host (Claude Code `mask` without `injectHosts` injects to every
      allowed domain).
- **Costs and limits.**
  - HTTP(S) only.
  - TLS interception needs a CA in the client's trust store (`SSL_CERT_FILE`, `NODE_EXTRA_CA_CERTS`,
    etc.). Certificate-pinned clients need a blind tunnel, where no injection happens.
  - Clients must honour `HTTPS_PROXY`.
  - Request-signing schemes need protocol-specific re-signing.
  - Non-HTTP protocols (IMAP/SMTP, database wire protocols, SSH) aren't covered.

## 4. Environment injection at launch

- **[`op run`](https://developer.1password.com/docs/cli/reference/commands/run/).** Scans the
  environment for `op://` references, resolves them, and runs the command "in a subprocess with the
  secrets made available as environment variables only for the duration of the subprocess".
  "Secrets printed to stdout or stderr are concealed by default", so `printenv DB_PASSWORD` shows
  `<concealed by 1Password>`, and `--no-masking` disables this. 1Password recommends service
  accounts scoped to specific vaults to limit what a session can resolve.
- **[Vault Agent process supervisor](https://developer.hashicorp.com/vault/docs/agent-and-proxy/agent/process-supervisor).**
  - `env_template` blocks render secrets into environment variables of the child in the `exec`
    block, and the child starts only after every template has rendered once.
  - With `restart_on_secret_changes = "always"` (the default), the child restarts when a secret
    changes or a dynamic secret nears expiry. Stdin, stdout, and stderr are forwarded.
- **[systemd credentials](https://systemd.io/CREDENTIALS/).**
  - systemd calls environment variables "problematic because by default they are inherited down the
    process tree". Credentials are instead files under `$CREDENTIALS_DIRECTORY`, readable only by
    the service's user and "not propagated down the process tree".
  - They are "placed in non-swappable memory" (ramfs) and optionally encrypted with a key from TPM2
    and/or `/var/`. With filesystem namespacing they are invisible to other services.
  - Set with `LoadCredential=`, `ImportCredential=`, or `SetCredentialEncrypted=`. They can also
    come from the hypervisor or cloud IMDS.
- **[Codex cloud](https://learn.chatgpt.com/docs/environments/cloud-environment).** "Secrets … are
  only available to setup scripts. For security reasons, secrets are removed before the agent phase
  starts." This is time-based separation: install with credentials, then hand an environment
  without them to the agent.
- **MCP stdio.** "Implementations using an STDIO transport SHOULD NOT follow this specification, and
  instead retrieve credentials from the environment"
  ([MCP authorization](https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization)).
  Giles follows this: the core sets stdio toolset `env` values on the trampoline and lists them in
  `inherit_env`, so values never touch the spec file or the command line
  (`Docs/technical/sandbox.md`, Environment).
- **What leaks if a tool echoes.**
  - The process has plaintext, so these all reveal it: `env`/`printenv`, `/proc/self/environ`, a
    crash or debug dump, a verbose HTTP log, and a "show config" tool.
  - Children inherit the environment, unless the value is a systemd credential file. Even then,
    `cat $CREDENTIALS_DIRECTORY/name` from within the service reveals it.
  - `op run` masking applies only to that subprocess's stdout and stderr, and only to exact values.
    Text the process sends over the network (for example, a stdio MCP server's JSON-RPC tool
    result) is a different channel. [INFERENCE] Stdout *is* the MCP stdio channel, so `op run`'s
    masking would rewrite protocol bytes, so it would be surprising rather than protective there.
    I didn't test this.
- **Value.** It keeps secrets off disk and out of config, rotates them through restarts, and limits
  them to one process. It does not hide them from the process.

## 5. Secret references resolved at use

- **1Password `op://vault/item/[section/]field`.** Resolved by `op read`, `op run`, or `op inject`
  ([1Password](https://developer.1password.com/docs/cli/secret-references/)). The reference is safe
  to show the model. Resolution needs an authenticated session or service account. [INFERENCE] If
  the tool process (or the agent's shell) holds that session, it can resolve *any* reference in its
  scope, so scope the service account.
- **n8n.** Credentials are "encrypted … before they get saved to the database" with an instance
  encryption key
  ([n8n](https://docs.n8n.io/deploy/host-n8n/configure-n8n/basic-configuration/configuration-examples/set-a-custom-encryption-key)).
  External vault values use `{{ $secrets.<vault>.<name> }}` and "only resolve in credential fields,
  not in any other fields supporting expressions"
  ([n8n external secrets](https://docs.n8n.io/administer/manage-credentials/use-external-secret-stores)).
  For AI agents, the model fills tool *parameters* through `$fromAI()`
  ([n8n](https://docs.n8n.io/build/integrate-ai/ai-examples/use-ai-for-parameters)), while the
  credential is attached to the tool node by the workflow author. [INFERENCE from those docs: the
  model never handles the credential. I found no n8n page saying so outright.]
- **OpenClaw SecretRef** (`env`, `file`, `exec`, `store`). Values resolve eagerly into an in-memory
  snapshot. Model-provider credentials travel as process-local sentinels until egress
  ([OpenClaw](https://docs.openclaw.ai/gateway/secrets/runtime-model)). OpenClaw is explicit that
  SecretRefs "are not a process-isolation boundary … Tool results and transcripts do not
  secret-mask that content."
- **GitHub Actions secrets** are injected only into jobs that reference them, and runners redact
  them from logs. Other values can be registered with `::add-mask::VALUE`. "Each masked word
  separated by whitespace is replaced with the `*` character"
  ([workflow commands](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-commands)).
  Redaction failures are covered in §8.
- **Giles today.** `{ secret = "name" }` / `{ env = "NAME" }` are resolved by the core before
  `Configure` or launch. Values are `Secret` (Debug and Display print `<redacted>`), and
  `PluginConfig` redacts by key path (`Docs/technical/config.md`, Secrets).
- **What leaks if a tool echoes.** The reference itself is harmless. The resolved value is in the
  consuming process (tier B), unless resolution happens in a proxy (tier A).

## 6. How comparable agent products keep credentials out of the model's context

| Product | Mechanism | Tier | Source |
| --- | --- | --- | --- |
| MCP (spec 2026-07-28) | HTTP: the client (host) holds OAuth tokens bound to the server's audience (RFC 8707). Servers "MUST NOT accept or transit any other tokens" and must not pass client tokens upstream. stdio: credentials come from the environment | A (HTTP) / B (stdio) | [authorization](https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization), [security best practices](https://modelcontextprotocol.io/specification/2026-07-28/basic/security_best_practices) |
| MCP elicitation | "Servers MUST NOT use form mode elicitation to request sensitive information such as passwords, API keys, access tokens"; URL mode is required, and "sensitive credentials never pass through the LLM context, MCP client or any intermediate MCP servers" | A (entry path) | [elicitation](https://modelcontextprotocol.io/specification/2026-07-28/client/elicitation) |
| OpenClaw | SecretRefs; a protected store that is write-only after saving (no reveal RPC); model-provider sentinels; opt-in egress proxy with host binding; a masked `secrets` tool prompt where "the value never enters the chat, the transcript, or the model's context". MCP server `env` accepts SecretRefs resolved before spawn. Sandbox SSH keys are written to 0600 temp files | A for provider calls and egress proxy; B for MCP env | [overview](https://docs.openclaw.ai/start/why-openclaw/secrets), [store/egress](https://docs.openclaw.ai/gateway/secrets/secret-store-and-egress), [integration examples](https://docs.openclaw.ai/gateway/secrets/integration-examples) |
| n8n | Encrypted credential store bound to nodes; the model supplies only `$fromAI` parameters; external secrets resolve only in credential fields | B (node process); model kept out by design [INFERENCE] | §5 links |
| Claude Code (CLI) | Permission deny rules such as `Read(./.env)` ([settings](https://code.claude.com/docs/en/settings)). Sandbox `credentials` `deny` blocks files and unsets env vars for sandboxed Bash; `mask` uses sentinels plus proxy injection. `CLAUDE_CODE_SUBPROCESS_ENV_SCRUB=1` strips credentials (by name or value) from Bash, hooks, and **stdio MCP servers** ([env vars](https://code.claude.com/docs/en/env-vars)). "There is no built-in credential deny list" | A (`mask`) / denial | [sandboxing](https://code.claude.com/docs/en/sandboxing) |
| Claude Code on the web | No git credentials or signing keys in the sandbox; a git proxy checks a scoped credential and the push target, then attaches the real token | A | [Anthropic engineering](https://www.anthropic.com/engineering/claude-code-sandboxing) |
| Codex cloud | Secrets only during setup; removed before the agent phase | Time-separated B | [OpenAI](https://learn.chatgpt.com/docs/environments/cloud-environment) |
| Deno Sandbox | Placeholder env plus host-bound substitution at egress | A | [Deno](https://docs.deno.com/sandbox/security) |

General guidance: OWASP LLM07:2025 says credentials "should not be contained within the system
prompt" and should be externalized "to the systems that the model does not directly access"
([OWASP](https://github.com/OWASP/www-project-top-10-for-large-language-model-applications/blob/main/2_0_vulns/LLM07_SystemPromptLeakage.md)).
Willison's "lethal trifecta" is private data, untrusted content, and external communication in one
agent. Any secret the agent can read can be exfiltrated through any outbound channel
([simonwillison.net](https://simonwillison.net/2025/Jun/16/the-lethal-trifecta)).

## 7. What leaks when the tool echoes its input or output

| Pattern | `env` | `cat ~/.ssh/id_*` / credential file | Tool echoes its own config or request | Residual risk |
| --- | --- | --- | --- | --- |
| ssh-agent (local) | `SSH_AUTH_SOCK` path only | Nothing if keys aren't on disk | n/a | The socket is a signing oracle for anything that can reach it |
| Agent forwarding to a workspace | Forwarded socket path | Nothing | n/a | Anything in the workspace (including root) can authenticate as you while the session lives; destination constraints narrow the targets only |
| SSH cert, key in Giles core | Nothing | Nothing in the workspace | n/a | None in the workspace; core compromise is out of this scope |
| SSH cert, key in workspace | Nothing | Private key and certificate | n/a | Bounded by TTL, principals, `source-address` |
| Injecting proxy / sentinel | Sentinel, proxy URL, CA path | Sentinel copy (Claude Code on Linux) | Sentinel | Reflection by an allowed host; injection to over-broad hosts; readable proxy CA key; non-HTTP paths unprotected |
| Env injection (`op run`, Vault Agent, Giles `inherit_env`) | **Plaintext** | n/a | **Plaintext** | Child inheritance; `/proc/self/environ`; dumps |
| systemd credential file | `$CREDENTIALS_DIRECTORY` path | **Plaintext** via the file | **Plaintext** | Not inherited by children; per-service visibility |
| Reference resolved by the tool | Reference, plus the resolver's session token if present | n/a | Plaintext after resolve | A tool holding a resolver session can resolve other references in scope |
| Remote MCP / OAuth held by Giles core | Nothing in the tool | n/a | Nothing (the token isn't in the LLM's context) | A server reflecting its own upstream token |
| EC2 instance profile | Nothing | n/a | `curl` to IMDS returns `AccessKeyId`/`SecretAccessKey`/`Token` ([AWS](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/instance-metadata-security-credentials.html)) | Any process on the instance, including the agent's shell |

## 8. Output scrubbing: where it helps and where it fails

**Helps:**

- Catches accidental echoes of known values in logs, stderr, error messages, and tool results:
  `op run` masking, GitHub log redaction, Giles `scrub`.
- OpenClaw registers resolved values "for exact-value log redaction as a defense in depth measure"
  ([OpenClaw](https://docs.openclaw.ai/gateway/secrets/runtime-model)).
- Matching by name *or* value catches unregistered variables to some degree (Claude Code's subprocess
  scrub "recognizes a credential by its variable name or its value"
  ([env vars](https://code.claude.com/docs/en/env-vars))).

**Fails** (GitHub: "Because there are multiple ways a secret value can be transformed, automatic
redaction is not guaranteed"; redaction "largely relies on finding an exact match"
([GitHub secure use](https://docs.github.com/en/actions/reference/security/secure-use))):

- **Encodings.** Base64, URL encoding, hex, JSON string escaping (`\/`, `\u00..`), and HTML entities
  aren't matched unless each variant is registered. GitHub says to "register the new value as a
  secret too" (same page).
- **Derived values.** A JWT signed with the key, or a session token returned by a login, is unknown
  to the scrubber. GitHub: "be sure to register that JWT as a secret or else it won't be redacted".
- **Structured secrets.** "Never use structured data as a secret": JSON, XML, or YAML blobs
  "significantly reduce the probability the secrets will be properly redacted" (same page). A PEM key
  printed with different line wrapping or indentation also won't match exactly [INFERENCE].
- **Partial matches.** A prefix or suffix (for example `head -c 20`, or a log line cut off at a
  width), truncation (Giles cuts tool inputs and results to 2,000 characters in compaction prompts:
  `Docs/technical/agent-loop.md`), or a secret split across stream chunks or words. GitHub masks per
  whitespace-separated word
  ([workflow commands](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-commands)).
  OpenClaw's substitution needs a "bounded carry window" to handle sentinels that cross chunk
  boundaries ([OpenClaw](https://docs.openclaw.ai/gateway/secrets/secret-store-and-egress)). A
  scrubber needs the same for streamed tool output [INFERENCE].
- **Short values.** Giles skips values under `MIN_SCRUB_LEN` (4 bytes) to avoid garbling
  (`Docs/technical/config.md`). Scrubbing every short or common value garbles output, and leaving
  them leaks them.
- **The model as an encoder.** A prompt-injected agent, or a hostile MCP server, can transform a
  value in arbitrary ways: reversing it, spacing it out, translating it, or splitting it across calls.
  No string matcher keeps up [INFERENCE].
- **Timing and channel.** Scrubbing protects only the surfaces it runs on. A secret a tool sends
  straight to an attacker's URL never passes through it, and a value that reaches the model before
  scrubbing is already compromised (provider logs, transcripts, memory). In Giles today, `scrub`
  covers `Configure` errors and plugin and MCP stderr. **The docs don't cover tool results going to
  the model** (`Docs/technical/config.md`, Secrets; nothing found in `Docs/`).

## 9. Fit for Giles

### Stdio MCP servers (sandboxed)

- **Current state.** Tier B. Credentials go to the server's environment through `inherit_env`; the
  trampoline uses `env_clear()` plus an allowlist, and the sandbox keeps processes from reading other
  processes' `environ` (`Docs/technical/sandbox.md`). A compromised server, or a benign one with a
  shell or file tool, can return its own environment in a tool result.
- **Cheapest improvement: scrub tool results** for the instance's own `env`, `auth`, and config
  secret values before they reach the model and the transcript. It is subject to every §8 failure,
  but it catches accidental echoes.
- **Tier A for HTTP-API servers: a core-hosted, sentinel-substituting egress proxy.**
  - Constraints from the sandbox design:
    - Network grants are `true` or a TCP port list, with no host filter. So the proxy can be the
      only route out only if `network = false` and the server reaches the proxy through a `unix`
      grant.
    - On Linux, per-socket restriction needs Landlock ABI 9; below that, any `unix` grant allows
      every reachable socket. macOS Seatbelt enforces exact paths
      (`Docs/technical/sandbox.md`, Network and sockets).
    - Most HTTP clients can't use a Unix-socket proxy directly. Claude Code's sandbox routes traffic
      "through a unix domain socket connected to a proxy server running outside the sandbox"
      ([Anthropic](https://www.anthropic.com/engineering/claude-code-sandboxing)), which needs a
      bridge inside the sandbox [INFERENCE, implementation not read].
    - The env allowlist (`GILES_*`, `LANG`, `inherit_env` names) would have to carry the
      `HTTPS_PROXY` and CA-bundle variables.
    - TLS termination needs a per-run CA that the core holds.
  - **Doesn't help:** non-HTTP protocols (IMAP/SMTP for an email toolset, databases), pinned
    clients, or servers that sign requests with the secret in-process.
- **Prefer HTTP MCP servers with OAuth held by the core where they exist.** The token stays in Giles
  (tier A against the model). Giles's remote toolsets already keep `auth.bearer` in the core
  (PLAN §7.10).
- **Never let an MCP server prompt for secrets through form elicitation.** Use URL mode or Giles's
  own secret-entry UI. This mirrors MCP's MUST NOT and OpenClaw's masked `secrets` tool.

### Agentic environments (#25: local or remote workspaces, e.g. EC2 over SSH)

- **Giles→workspace authentication.**
  - Keep the private key in the Giles core (in process, or a Giles-owned agent) and never in the
    workspace.
  - Use short-lived certificates: a local CA for the local provider, the Vault SSH engine for the
    Vault provider. Set `force-command`/`source-address` as needed, and leave agent forwarding off.
  - EC2 Instance Connect is an AWS-native alternative: a 60-second public-key push, with IAM
    controlling who may push.
- **Credentials the work inside the workspace needs** (git push, package registries, cloud APIs),
  strongest first:
  1. **Proxy with substitution, outside the workspace** (Claude Code web git proxy, OpenClaw,
     Deno). It is reached from the workspace through `ssh -R remote_socket:local_socket` (or a TCP
     port), which forwards "connections to the given TCP port or Unix socket on the remote (server)
     host … to the local side" ([ssh(1)](https://man.openbsd.org/ssh.1)). It can also enforce
     content policy, such as pushes only to a configured branch.
  2. **Setup-only secrets** (Codex): provision with credentials, then strip them before the agent
     runs.
  3. **Short-lived scoped tokens** given to the workspace, accepting that they leak. Example:
     GitHub App installation tokens "expire after 1 hour" and can be limited to `repositories`
     ([GitHub](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/authenticating-as-a-github-app-installation)).
  4. **Agent forwarding:** usable by anything in the workspace for the whole session.
     - `-c` confirmation needs a custom agent to be meaningful for a daemon (possible with
       `ssh-agent-lib`).
     - Destination constraints need OpenSSH ≥ 8.9 on all hops and limit only SSH user-auth targets.
- **Cloud identity on the instance.**
  - Instance-profile credentials are available to every process through IMDS; treat them as the
    agent's own. [INFERENCE] Give the role only what the agent may do.
  - systemd credentials from IMDS ([systemd](https://systemd.io/CREDENTIALS/)) don't change this:
    the workspace processes are the consumers.

### Windows (later)

Not researched here. The Unix-socket-based patterns (ssh-agent, Unix-socket proxy grants) would need
Windows equivalents. Unverified.
