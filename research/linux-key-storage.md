# Ticket #62: Linux options for keeping a secret-store key on a headless host

Question: a headless Linux daemon (systemd service, plain `giles serve`, or the container image) needs a key that encrypts Giles's local secret store. Where can it keep that key so it isn't on disk in plain form, and what does each option need?

Researched 2026-10-28. Crate versions and dates come from the crates.io API on that day. "Plugin" below means a gRPC channel plugin or stdio MCP server running under Giles's sandbox. The sandbox runs as the **same UID** as the core (Docs/technical/sandbox.md).

## Summary

- **No mechanism protects the key from root on the running host, and none protects it from code running as Giles's UID outside the sandbox.** The differences between mechanisms are about disk and backup theft, other local users, and what happens at boot and restart. Giles's own sandbox is what keeps a compromised plugin away from the key. The sandbox already returns `EPERM` for `keyctl`/`add_key`/`request_key`, `ptrace` and `process_vm_*`. It grants no `/run` or `/proc`, only `/dev/{null,random,urandom,zero}`, and no `AF_UNIX` without a `unix` grant.
- **`systemd-creds` / `LoadCredentialEncrypted=` is the strongest unattended option on a systemd host.** It uses AES-256-GCM, keyed by a TPM2-derived key, the root-only host key `/var/lib/systemd/credential.secret`, or both. "Both" is the default when a TPM2 is present and `/var` is persistent. At service start systemd decrypts the credential into a read-only, unswappable (ramfs) file under `$CREDENTIALS_DIRECTORY`, readable only by the unit's user. Restarts and reboots need no human. With the TPM, a stolen disk alone is useless. With the host key only, it protects against backup theft only if backups skip `/var/lib/systemd/credential.secret`. User services need systemd ≥ 256 (`--user`). Giles needs no crate: it reads one file.
- **The kernel keyring (`keyctl`) is memory-only and never survives a reboot.** The user keyring disappears when the UID's last process exits. The persistent keyring expires 3 days after its last access by default. Every process of the same UID can read keys in the user or persistent keyring: a `KEY_SPEC_USER_KEYRING` lookup counts as possession. Docker's default seccomp profile denies `keyctl`/`add_key`/`request_key`, and Podman's denies `add_key`/`request_key`. So the keyring can only be a restart cache on a bare or VM host, not a store. Crate: `linux-keyutils` 0.2.5 (and the `linux-keyutils-keyring-store` 1.0.0 store for `keyring-core`).
- **Secret Service (gnome-keyring, oo7, KeePassXC via libsecret/D-Bus) is a poor fit headless.**
  - It needs a D-Bus session bus. A system service has none unless it runs under a lingering user manager (`loginctl enable-linger`), which provides `$XDG_RUNTIME_DIR/bus`, or starts its own with `dbus-run-session`.
  - The collection is unlocked at login by a PAM module using the login password, which a headless service account never enters. The alternatives are a password on stdin (`gnome-keyring-daemon --unlock`, `oo7-daemon --login`) or, for oo7 on systemd ≥ 258, a `systemd-creds --user` credential. Either way it's a passphrase or `systemd-creds` again, one hop removed.
  - Once a collection is unlocked, any process that can talk to the session bus can read every secret in it.
  - KeePassXC's provider is its GUI app with the database unlocked.
  - Crates: `oo7` 0.6.0 (pure Rust, D-Bus and file backends), `secret-service` 5.2.0, and the `dbus-`/`zbus-secret-service-keyring-store` 1.0.1 stores.
- **Direct TPM2 sealing** (a sealed blob in `GILES_HOME`, unsealed at start) works unattended and defeats disk and backup theft. It needs `/dev/tpmrm0`, which is mode 0660, group `tss`, under tpm2-tss's udev rules. Without a PCR policy or auth value, anyone who can read the blob and open the TPM can unseal it. Containers have no TPM unless the device is passed in. The Rust stack is `tss-esapi` 7.7.0, which links the C tpm2-tss ≥ 4.1.3 (a `bundled` feature exists); the pure-Rust `tpm2-protocol` 1.2.3 is only a marshaller. On a systemd host, `systemd-creds` gives the same TPM binding with far less code in Giles.
- **A passphrase or key file supplied at start is the only option that works everywhere:** bare host, any systemd version, and the container image.
  - A passphrase (KDF, e.g. `argon2` 0.6.0; prompt via `rpassword` 7.5.4, or `systemd-ask-password`) defeats disk and backup theft completely.
  - It cannot restart unattended. A crash or restart needs re-entry unless the key is cached. Two cache options exist, neither surviving a reboot: the kernel keyring, readable by the same UID, or a memfd in systemd's fd store (`FileDescriptorStoreMax=`, `FileDescriptorStorePreserve=`).
  - A key file only moves the problem to wherever that file lives.
- **Container-native secrets put the key in a file under `/run/secrets/<name>`.** That file is tmpfs for Swarm and the Kubernetes kubelet, and a bind mount or copy for Compose and Podman. At-rest protection is the orchestrator's:
  - Swarm: an encrypted Raft log, plus optional autolock, which needs an unlock key when a manager restarts.
  - Kubernetes: etcd holds Secrets unencrypted by default; encryption at rest or a KMS v2 provider is needed.
  - Podman's default `file` driver: a "read-protected file".
  - Compose without Swarm: a plain host file.

  They restart unattended. Swarm "secrets" are Swarm-only; standalone `docker run` has no secrets feature.
- **What the sandbox must keep blocking:**
  - Keep `keyctl`/`add_key`/`request_key` denied. A plugin that could call them could read a key cached in the user or persistent keyring, or in an inherited session keyring.
  - Never grant `/run/credentials`, `$CREDENTIALS_DIRECTORY`, `/run/secrets`, `/run/user/<uid>` (the D-Bus session bus), `/run/dbus/system_bus_socket`, `/dev/tpm*`, or `GILES_HOME`'s key/blob files.
  - Below Landlock ABI 9, any `unix` grant lets a plugin connect to every reachable pathname socket, the session bus included. Below ABI 6 that extends to abstract sockets. So with Secret Service, a plugin with any `unix` grant could reach an unlocked keyring on older kernels.
  - Keep the core's environment scrubbed, as it already is (`CREDENTIALS_DIRECTORY`, any key variables).

## How to read the threat columns

- **Disk/backup theft**: an attacker holds a copy of the disk or a backup, not the running machine.
- **Another local user**: a different UID on the same host, not root.
- **Compromised plugin**: code inside Giles's sandbox, running as Giles's UID. All mechanisms rely on the sandbox here (see [Sandbox implications](#sandbox-implications-for-giles)), because without it a same-UID process can read every one of them.
- **Root on the running host** can read every option: it can read `/proc/<pid>/mem`, every credential file, every keyring, and unseal the TPM. None is listed as protecting against it.

## 1. Secret Service API (libsecret, GNOME Keyring, oo7, KeePassXC)

What it is: the freedesktop [Secret Service API](https://specifications.freedesktop.org/secret-service-spec/latest/) (`org.freedesktop.secrets`), served over D-Bus by a per-user daemon. The [ArchWiki](https://wiki.archlinux.org/title/GNOME/Keyring) describes it as "a service running in the user's login session". Providers:

- gnome-keyring.
- [oo7](https://wiki.archlinux.org/title/Oo7), the Rust replacement, which ArchWiki says has been GNOME's default key storage since GNOME 51.
- KWallet.
- [KeePassXC](https://raw.githubusercontent.com/keepassxreboot/keepassxc/develop/docs/topics/SecretService.adoc).

**Without a desktop session or session bus.**
- A session bus is required. On systemd systems, the user manager's `dbus.socket` listens on `%t/bus`, i.e. `$XDG_RUNTIME_DIR/bus`, and exports `DBUS_SESSION_BUS_ADDRESS` ([dbus.socket](https://gitlab.freedesktop.org/dbus/dbus/-/raw/main/bus/systemd-user/dbus.socket.in)). A user manager runs without anyone logged in only with lingering: "a user manager is spawned for the user at boot and kept around after logouts" ([loginctl](https://www.freedesktop.org/software/systemd/man/latest/loginctl.html)).
- Otherwise a daemon can start a private bus with [`dbus-run-session`](https://dbus.freedesktop.org/doc/dbus-run-session.1.html), which lasts as long as its child.
- [INFERENCE] A plain system service (`User=giles`) therefore has no session bus by default.

**Unlocking.**
- The login keyring is unlocked by `pam_gnome_keyring.so` / `pam_oo7.so` capturing the login password ([ArchWiki GNOME/Keyring](https://wiki.archlinux.org/title/GNOME/Keyring), [ArchWiki oo7](https://wiki.archlinux.org/title/Oo7)). A headless service account never logs in interactively.
- Alternatives:
  - `gnome-keyring-daemon --unlock`: "Read a password from stdin, and use it to unlock the login keyring" ([gnome-keyring-daemon(1)](https://manpages.debian.org/unstable/gnome-keyring/gnome-keyring-daemon.1.en.html)).
  - `oo7-daemon --login`: password on stdin.
  - `oo7-cli unlock`.
  - On systemd ≥ 258, oo7-daemon's unit imports `oo7.keyring-encryption-password` from the user's `systemd-creds --user` credential store ([ArchWiki oo7](https://wiki.archlinux.org/title/Oo7)). The same page warns: "Any user capable of reading the credential file and with access to the TPM (e.g. the root user) can decrypt the blob."
- A blank keyring password stores the contents unencrypted ([ArchWiki](https://wiki.archlinux.org/title/GNOME/Keyring)).
- KeePassXC exposes the database group configured in its GUI settings, and only "while running", with the database open ([KeePassXC docs](https://raw.githubusercontent.com/keepassxreboot/keepassxc/develop/docs/topics/SecretService.adoc)). Unverified: I found no documented headless mode for KeePassXC's Secret Service provider.

**Protection.**

| Threat | Result |
| --- | --- |
| Disk/backup theft | Yes, if the keyring has a non-empty password. Files live in `~/.local/share/keyrings/`. |
| Another local user | Yes. Per-user bus and files; [INFERENCE] `$XDG_RUNTIME_DIR` is per-user. |
| Same-UID process | **No** once unlocked: "Any application [can easily read any secret] if the keyring is unlocked" (CVE-2018-19358). GNOME's position is that untrusted applications must not be allowed to talk to the secret service. Only Flatpak-style bus filtering helps ([ArchWiki](https://wiki.archlinux.org/title/GNOME/Keyring), [ArchWiki oo7](https://wiki.archlinux.org/title/Oo7)). |
| Compromised plugin | Only as long as the sandbox denies the bus socket (see below). |

**At boot / unattended restart.**
- It needs a lingering user manager (or `dbus-run-session`), the daemon, and an unlock secret.
- Unattended only if that unlock secret is itself stored somewhere unattended, e.g. a `systemd-creds --user` credential, which makes Secret Service an extra layer over `systemd-creds`.
- The container image has none of this. [INFERENCE] You would have to run dbus plus a keyring daemon inside the container.

**Rust crates.**

| Crate | Notes |
| --- | --- |
| [`oo7`](https://docs.rs/oo7/latest/oo7/) 0.6.0 (2026-09-21) | Pure Rust, async-only. `dbus` backend, plus a `file` backend "compatible with libsecret" whose key can come from the Secret portal. |
| [`secret-service`](https://docs.rs/secret-service/latest/secret_service/) 5.2.0 (2026-08-29) | zbus-based, async and blocking APIs. |
| `keyring-core` 1.0.1 (2026-10-13) with `dbus-secret-service-keyring-store` 1.0.1 or `zbus-secret-service-keyring-store` 1.0.1 | The `keyring` crate 4.2.0 now says apps "should instead be linking to the keyring-core library and any specific credential stores they want to use" ([docs.rs keyring](https://docs.rs/keyring/latest/keyring/)). |

## 2. Linux kernel keyring (`keyctl`, `add_key`, `request_key`)

**What it is.** In-kernel key retention in unswappable, quota-controlled memory ([keyrings(7)](https://man7.org/linux/man-pages/man7/keyrings.7.html)). The `user` type holds up to 32,767 bytes and is readable from user space. The `logon` type cannot be read back by user space at all; it exists for keys only the kernel consumes.

**Persistence.**
- None across reboots: "completely in-memory and will not persist across reboots" ([linux-keyutils-keyring-store docs](https://docs.rs/linux-keyutils-keyring-store/latest/linux_keyutils_keyring_store/)).
- The user keyring lives "so long as there are processes running with that real UID" ([user-keyring(7)](https://man7.org/linux/man-pages/man7/user-keyring.7.html)).
- The persistent keyring outlives the UID record for cron-style use. It expires after `/proc/sys/kernel/keys/persistent_keyring_expiry`, default 259200 s (3 days), and the timer resets on each `keyctl_get_persistent` ([persistent-keyring(7)](https://man7.org/linux/man-pages/man7/persistent-keyring.7.html), [keyrings(7)](https://man7.org/linux/man-pages/man7/keyrings.7.html)).

**Access model.**
- A key created without explicit permissions gets possessor view/read/search/link/setattr (plus write) but **user: view only** ([kernel `key.c`](https://raw.githubusercontent.com/torvalds/linux/master/security/keys/key.c)).
- However, looking up `KEY_SPEC_USER_KEYRING` or `KEY_SPEC_USER_SESSION_KEYRING` marks the keyring as possessed (`make_key_ref(key, 1)`) ([kernel `process_keys.c`](https://raw.githubusercontent.com/torvalds/linux/master/security/keys/process_keys.c)). Possession then extends to every key linked from it ([keyrings(7)](https://man7.org/linux/man-pages/man7/keyrings.7.html)). [INFERENCE] So **any process of the same UID** can read a key kept in `@u`, or in the persistent keyring, which it can link by itself.
- A key in a process keyring is private to that process but dies with it.
- systemd gives system services `KeyringMode=private` by default: a fresh session keyring "not linked up with any user keyring", so services sharing a UID "do not share their key material" through the session keyring ([systemd.exec(5)](https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html)). [INFERENCE] It doesn't stop explicit `@u` access.

**Containers.**
- Docker's documentation says it blocks `keyctl`, `add_key` and `request_key`: "Prevent containers from using the kernel keyring, which is not namespaced" ([Docker seccomp](https://docs.docker.com/engine/security/seccomp/)).
- Podman's default profile ([containers/common seccomp.json](https://raw.githubusercontent.com/containers/common/main/pkg/seccomp/seccomp.json)) allows `keyctl` but leaves `add_key`/`request_key` unlisted. Unlisted calls fail with `ENOSYS`.

**Protection.**

| Threat | Result |
| --- | --- |
| Disk/backup theft | Yes: never written to disk. Swap: `user` keys are unswappable; `big_key` payloads in tmpfs are encrypted since Linux 4.8. |
| Another local user | Yes (UID-based permissions). |
| Same-UID process | No (see above). |
| Compromised plugin | Only because the sandbox's seccomp returns `EPERM` for `keyctl`/`add_key`/`request_key`, which it does today (Docs/technical/sandbox.md). |

**At boot / unattended restart.** Something must put the key in at every boot. As a **restart cache** it lets `systemctl restart` or a crash restart skip a passphrase prompt. `systemd-ask-password --keyname=` does exactly this, caching in root's keyring with a 2.5-minute timeout ([systemd-ask-password(1)](https://www.freedesktop.org/software/systemd/man/latest/systemd-ask-password.html)). It is not usable in the container image under default profiles.

**Kernel `trusted` and `encrypted` key types.**
- Created in the kernel. "user space sees, stores, and loads only encrypted blobs", and `trusted` keys are sealed by a trust source such as a TPM ([kernel docs](https://www.kernel.org/doc/html/latest/security/keys/trusted-encrypted.html)).
- [INFERENCE] Giles can't get the plaintext back to use as a user-space AES key. The only user-space consumer path is AF_ALG's `ALG_SET_KEY_BY_KEY_SERIAL`, and the kernel now calls AF_ALG "insecure and … deprecated", "supported only for backwards compatibility" ([kernel crypto user-space interface](https://www.kernel.org/doc/html/latest/crypto/userspace-if.html)).
- Not a fit.

**Rust crates.**

| Crate | Notes |
| --- | --- |
| `linux-keyutils` 0.2.5 (2026-03-21) | Safe wrapper over the raw syscalls; no libkeyutils. |
| `keyutils` 0.4.0 (2023-08-17) | Older; binds libkeyutils. |
| `linux-keyutils-keyring-store` 1.0.0 | For `keyring-core`. Its [docs](https://docs.rs/linux-keyutils-keyring-store/latest/linux_keyutils_keyring_store/) recommend it for headless boxes, but explicitly as "a secure cache", and suggest re-prompting or `systemd-ask-password` to reload. |

## 3. systemd credentials: `systemd-creds`, `LoadCredentialEncrypted=`, `SetCredentialEncrypted=`, `ImportCredential=`

**Mechanism.** `systemd-creds encrypt` produces an AES-256-GCM, Base64 blob, "keyed by a SHA256 hash of one or both" of the following keys ([systemd-creds(1)](https://www.freedesktop.org/software/systemd/man/latest/systemd-creds.html)):

1. A key derived from the TPM2: "not stored on the host system … can only be decrypted again by the local machine".
2. The host key `/var/lib/systemd/credential.secret`, "only accessible to the root user".
3. Both. This is the default when a TPM2 is present, not in a container, and `/var/lib/systemd/` is persistent.

More from the same page:
- `--with-key=null` gives neither confidentiality nor authenticity.
- The credential name is embedded and checked at decrypt time.
- `--not-after=` sets an expiry.
- TPM binding can add `--tpm2-pcrs=` (no PCRs by default) or a signed PCR policy (`--tpm2-public-key=`, default PCR 11).
- systemd v262 also pins credentials to the TPM's SRK against interposer attacks.

**Delivery.**
- `LoadCredentialEncrypted=ID:PATH` decrypts at service activation and places plaintext in `$CREDENTIALS_DIRECTORY`, "a read-only location that (if possible and permitted) is backed by non-swappable memory". The data is "only accessible to the user associated with the unit … (as well as the superuser)". There is a 1 MB per-unit limit ([systemd.exec(5)](https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html)).
- Credentials are "not propagated down the process tree", and with mount namespacing (e.g. `PrivateMounts=`, implied by `ProtectSystem=`) they are invisible to other services ([systemd.io/CREDENTIALS](https://systemd.io/CREDENTIALS/)).
- Search paths with a relative source: `/etc/credstore.encrypted/` and similar. User managers use `$XDG_CONFIG_HOME/credstore.encrypted/` and similar ([systemd.exec(5)](https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html)).
- `SetCredentialEncrypted=` embeds the ciphertext in the unit file. That is safe even though unit files are world-readable ([systemd.io/CREDENTIALS](https://systemd.io/CREDENTIALS/)).

**User services.**
- systemd 256 added `--user`/`--uid=`: "Encrypted service credentials can now be made accessible to unprivileged users" ([systemd NEWS](https://raw.githubusercontent.com/systemd/systemd/main/NEWS)).
- The key mixes in the UID, the user name and `machine-id`. Per-user managers "cannot decrypt secrets intended for the system or for other users" ([systemd-creds(1)](https://www.freedesktop.org/software/systemd/man/latest/systemd-creds.html), [systemd.exec(5)](https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html)).

**Containers.**
- `--with-key=auto` skips the TPM "in a container" ([systemd-creds(1)](https://www.freedesktop.org/software/systemd/man/latest/systemd-creds.html)).
- A container manager can pass credentials to a systemd PID 1 by setting `$CREDENTIALS_DIRECTORY`; `/run/host/credentials` is recommended ([Container Interface](https://raw.githubusercontent.com/systemd/systemd/main/docs/CONTAINER_INTERFACE.md)).
- [INFERENCE] Giles's image presumably doesn't run systemd. It could still honour `$CREDENTIALS_DIRECTORY` if one is set, e.g. by `systemd-nspawn --load-credential`.

**Protection.**

| Threat | Result |
| --- | --- |
| Disk theft | With the TPM: the stolen disk is useless without the chip. Host key only: no, unless the disk is encrypted, since the key sits on the same disk. |
| Backup theft | Yes if the backup lacks the TPM, or lacks `credential.secret` in host-key mode. |
| Another local user | Yes: plaintext is 0400 to the unit's user; ciphertext needs root or the TPM. |
| Same-UID process outside the sandbox | No: it can read `/run/credentials/<unit>`. |
| Compromised plugin | Yes only because Landlock grants no `/run`, and the core's environment (including `CREDENTIALS_DIRECTORY`) is scrubbed (Docs/technical/sandbox.md). |

**At boot / unattended restart.** It is fully unattended: decrypted on every service start, after crashes, and after reboots. It needs systemd ≥ 250 for system services and ≥ 256 for user services. A PCR-bound credential needs re-enrolling (or a signed PCR policy) after firmware or bootloader changes.

**Rust.** Nothing is needed. Read `$CREDENTIALS_DIRECTORY/<name>`. The docs call this "the primary interface … since it also works for user services" ([systemd.exec(5)](https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html)).

## 4. Direct TPM2 use

**Mechanism.** Seal the store key to the TPM (optionally under a PCR policy and/or an auth value, i.e. a PIN) and keep the sealed blob in `GILES_HOME`. Unseal it at start. [INFERENCE] This is what systemd-creds does internally; doing it in Giles only adds value without systemd, or for policies systemd-creds doesn't expose.

**Device access.** tpm2-tss's udev rules set `/dev/tpm[0-9]*` to mode 0660 owned by user `tss`, and `/dev/tpmrm[0-9]*` to mode 0660 group `tss` ([tpm-udev.rules](https://raw.githubusercontent.com/tpm2-software/tpm2-tss/master/dist/tpm-udev.rules)). Giles's user would need the `tss` group. Distributions may differ (unverified).

**Containers and VMs.** [INFERENCE] There is no TPM in a container unless `/dev/tpmrm0` is passed in. Cloud VMs often have a virtual TPM 2.0, e.g. [AWS NitroTPM](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/nitrotpm.html) and [GCP Shielded VM](https://cloud.google.com/compute/shielded-vm/docs/shielded-vm).

**Protection.**

| Threat | Result |
| --- | --- |
| Disk/backup theft | Yes. |
| Whole-machine theft | Only with a PCR policy (e.g. Secure Boot state) or a PIN. Otherwise booting the machine normally and getting root is enough. |
| Another local user | Yes, if the blob is 0600 or the user isn't in `tss`. Without an auth value, anyone with the blob and TPM access can unseal. |
| Compromised plugin | The sandbox grants no `/dev/tpm*` (only `/dev/{null,random,urandom,zero}`), and below Landlock ABI 5 there is no device-ioctl rule (Docs/technical/sandbox.md). |

**At boot / unattended restart.** Unattended. It needs a TPM2 (or vTPM), the device permission, and the C library unless a pure-Rust stack is used.

**Rust crates.**

| Crate | Notes |
| --- | --- |
| [`tss-esapi`](https://docs.rs/tss-esapi/latest/tss_esapi/) 7.7.0 (2026-10-09) | Wraps the C TSS ESAPI. `tss-esapi-sys` needs `tss2-sys`, `tss2-esys`, `tss2-tctildr` and `tss2-mu` ≥ 4.1.3 via pkg-config, or a `bundled` feature that builds tpm2-tss (whose own dependencies still need pkg-config) ([tss-esapi-sys README](https://raw.githubusercontent.com/parallaxsecond/rust-tss-esapi/main/tss-esapi-sys/README.md)). |
| `tpm2-protocol` 1.2.3 | Pure-Rust TPM 2.0 marshaller only; sealing sessions and policies would be Giles's own code. |
| `tpm2` 0.1.0 (tpm-rs) | Early; 51 recent downloads. |

Related: [Clevis](https://raw.githubusercontent.com/latchset/clevis/master/README.md) is a "pluggable framework for automated decryption" with TPM2 and Tang (network-bound) pins, but it's a CLI/C tool, not a crate.

## 5. Passphrase or key file supplied at start

**Mechanisms.**
- An interactive passphrase on a TTY (`rpassword` 7.5.4), stretched with a KDF (`argon2` 0.6.0).
- Under systemd, `systemd-ask-password`: without a TTY it queues the question for agents (console, plymouth, wall, `systemd-tty-ask-password-agent`). The default timeout is 90 s, and `--timeout=0` waits forever. With `--credential=` it first tries a credential ([systemd-ask-password(1)](https://www.freedesktop.org/software/systemd/man/latest/systemd-ask-password.html)).
- A key passed on stdin or an inherited fd.
- A key file on removable or network media.
- An environment variable, which leaks to `/proc/<pid>/environ` for the same UID. Giles's sandbox already scrubs the environment (Docs/technical/sandbox.md).

**Keeping it across restarts without a human.** Two options; neither survives a reboot.
- The kernel keyring (§2): the same UID can read it, and it doesn't work in containers.
- systemd's file-descriptor store. A service can push a `memfd` with `FDSTORE=1`, and systemd hands it back "on the next service restart". With `FileDescriptorStorePreserve=yes` or `on-success`, the store outlives a stop ([systemd.service(5)](https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html)). [INFERENCE] It needs `NotifyAccess=` and the `sd-notify` crate (0.5.0), which handles FDSTORE messages.

**In-memory hardening.** [`memfd_secret(2)`](https://man7.org/linux/man-pages/man2/memfd_secret.2.html) (Linux 5.14, default-on since 6.5) keeps a region out of the kernel's page tables, locks it against swap and blocks hibernation. Crates: `zeroize` 1.9.1 and `secrecy` 0.10.3.

**Protection.**

| Threat | Result |
| --- | --- |
| Disk/backup theft | Yes with a passphrase: nothing usable is on disk. A key file protects only if it lives elsewhere. |
| Another local user | Yes. |
| Compromised plugin | The key lives only in the core's memory. Landlock forbids `ptrace` and reading `/proc/<pid>/mem` of processes outside the sandbox domain ([Landlock docs](https://kernel.org/doc/html/latest/userspace-api/landlock.html), Docs/technical/sandbox.md), and seccomp denies `ptrace` and `process_vm_readv`. |

**At boot / unattended restart.** It needs a human (or an external system feeding the fd or stdin) at every boot. Every restart also needs one unless the key is cached as above. It works for a plain `giles serve`, any systemd host, and the container image (`docker run -i`, or a secret on stdin).

## 6. Container-native secrets

| Option | Delivery in container | At rest | Unattended restart | Source |
| --- | --- | --- | --- | --- |
| Docker Swarm secrets | Decrypted into an in-memory filesystem at `/run/secrets/<name>` | Raft log encrypted. With autolock the Raft keys need an unlock key "When Docker restarts" | Yes, without autolock; managers need the unlock key with it | [Swarm secrets](https://docs.docker.com/engine/swarm/secrets/), [autolock](https://docs.docker.com/engine/swarm/swarm_manager_locking/) |
| Docker standalone | "Docker secrets are only available to swarm services, not to standalone containers" | n/a | n/a | [Swarm secrets](https://docs.docker.com/engine/swarm/secrets/) |
| Docker Compose secrets | "Compose delivers each secret by bind-mounting a single file" at `/run/secrets/<name>` | Plain host file (or env var) unless Swarm | Yes | [Compose secrets](https://docs.docker.com/compose/how-tos/use-secrets/) |
| Podman secrets | Copied and mounted at `/run/secrets/<name>` (`type=mount`, default) or set as an env var (`type=env`) | Drivers: `file` (default, "a read-protected file"), `pass` (GPG-encrypted), `shell` (custom scripts) | Yes; `pass` needs the GPG key usable | [podman secret create](https://docs.podman.io/en/latest/markdown/podman-secret-create.1.html), [podman run --secret](https://docs.podman.io/en/latest/markdown/podman-run.1.html) |
| Kubernetes Secrets | The kubelet stores volume copies "into a tmpfs"; also env vars | "by default, stored unencrypted in the API server's underlying data store (etcd)". Encryption at rest, or a KMS v2 envelope with the KEK in a remote KMS, must be enabled | Yes | [K8s Secrets](https://kubernetes.io/docs/concepts/configuration/secret/), [KMS provider](https://kubernetes.io/docs/tasks/administer-cluster/kms-provider/) |

**Protection.**
- Disk and backup theft of the *container host* is the orchestrator's problem, per the "At rest" column.
- Another local user: file mode inside the container, plus container isolation.
- Compromised plugin: Landlock grants no `/run`, so `/run/secrets` is unreadable, and the env is scrubbed. Prefer `type=mount` over env.

**Rust.** Nothing: read a file. Unverified: there is no conventional variable naming `/run/secrets`; a `--key-file`/`*_FILE` option is the usual pattern.

## Comparison

| Mechanism | Disk theft | Backup theft | Other local user | Same UID, unsandboxed | Unattended reboot | Unattended restart | Container image | Main Rust dependency |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Secret Service | Yes (keyring password) | Yes | Yes | No once unlocked | Only via a stored unlock secret | Same | Impractical | `oo7` / `secret-service` |
| Kernel keyring | n/a (memory) | n/a | Yes | No | No (lost) | Yes (as cache) | Blocked by default seccomp | `linux-keyutils` |
| systemd-creds, TPM (+host) | Yes | Yes | Yes | No (plaintext in `/run/credentials`) | Yes | Yes | No TPM; plain `$CREDENTIALS_DIRECTORY` only | none (read file) |
| systemd-creds, host key only | No without disk encryption | Yes if the backup skips `credential.secret` | Yes | No | Yes | Yes | n/a | none |
| Direct TPM2 seal | Yes | Yes | Yes (blob 0600, `tss` group) | No (can unseal) | Yes | Yes | Only with a passed device | `tss-esapi` (C lib) |
| Passphrase at start | Yes | Yes | Yes | No (memory, if ptrace is allowed) | No | Only with a cache (keyring / fd store) | Yes (stdin) | `argon2`, `rpassword` |
| Key file / env at start | Depends where it lives | Same | Yes (0600) | No | Yes if the file is present | Yes | Yes | none |
| Container secrets | Orchestrator-dependent (K8s: unencrypted etcd by default) | Same | Yes | No | Yes | Yes | Yes | none |

## Sandbox implications for Giles

Current Linux sandbox state, from Docs/technical/sandbox.md:
- seccomp returns `EPERM` for `keyctl`, `add_key`, `request_key`, `ptrace`, `process_vm_readv`/`writev`, `io_uring_*`, and System V IPC.
- `AF_UNIX` sockets are allowed only with a `unix` grant.
- Landlock grants no `/run`, `/proc`, `/tmp` or home directories, and of `/dev` only null, random, urandom and zero.
- The environment is scrubbed.

What each option relies on:
- **Kernel keyring.** Keep the three key syscalls denied; that is essential if the core caches the key in `@u` or the persistent keyring. [INFERENCE] The trampoline could additionally join a fresh anonymous session keyring (`KEYCTL_JOIN_SESSION_KEYRING`) before installing the filter. Even then, a plugin that could call `keyctl` could still reach `@u` by its special ID.
- **systemd credentials and container secrets.** Never grant `/run/credentials/…`, `$CREDENTIALS_DIRECTORY`, or `/run/secrets`. Keep `CREDENTIALS_DIRECTORY` out of plugin environments; it already is, because only `GILES_*`, `LANG` and `inherit_env` names pass.
- **Secret Service.** The session bus is `/run/user/<uid>/bus`, and the system bus is `/run/dbus/system_bus_socket` [INFERENCE: standard path]. The sandbox doc says below Landlock ABI 9 "a plugin with any `unix` grant can connect to every socket it can reach", and below ABI 6 abstract sockets aren't scoped. `dbus-run-session`/`dbus-launch` buses may use abstract addresses [INFERENCE]. So with Secret Service in use, any plugin holding a `unix` grant on an older kernel could ask the unlocked keyring for the key.
- **TPM.** Never grant `/dev/tpm*` / `/dev/tpmrm*`, and keep the sealed blob unreadable to plugins (`GILES_HOME` isn't granted).
- **Key in core memory.** Landlock's implicit ptrace rule plus seccomp's `ptrace` and `process_vm_readv` denials already protect it. Keep them.
- **macOS.** Not covered here; the Seatbelt equivalents (Keychain/XPC) are another ticket's scope.

## Unverified or inferred

- KeePassXC: I found no documented headless Secret Service mode.
- That Giles's container image doesn't run systemd as PID 1 (assumed).
- The exact `/dev/tpmrm0` ownership on each distribution; only tpm2-tss's shipped udev rule was checked.
- A pure-Rust TPM sealing path: `tpm2-protocol` only marshals, and I didn't evaluate a full stack.
