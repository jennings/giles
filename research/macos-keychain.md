# Research: macOS Keychain access from the Giles daemon (ticket #63)

Question: can Giles, running as a launchd agent, a launchd daemon, or from a terminal, keep its secret-store key in the macOS Keychain without a prompt on every start or rebuild? What are the constraints?

Sources are Apple technotes, man pages, and DTS (Quinn "The Eskimo!") forum answers, plus crate docs. I also ran experiments on this machine (macOS 27.0, build 26A428, arm64), described in [Experiments](#experiments-run-on-this-mac). Experiment results are marked **[tested]**. Things I couldn't confirm are marked **[unverified]** and my own conclusions **[inference]**.

## Summary

- **Yes, in two configurations, and they come with very different constraints.**
  - (a) Giles runs as the user (terminal, or a LaunchAgent in the GUI session) and uses that user's **login keychain**, a file-based keychain. This works only after the user has logged in to the GUI, which unlocks it.
  - (b) Giles runs as a **root LaunchDaemon** and uses the **System keychain**. This works at boot, before anyone logs in, but the System keychain file is writable only by root.
  - A daemon running as a non-root user (`UserName=giles`) has neither in practice.
  - The **data protection keychain** is ruled out: it needs entitlements authorized by a provisioning profile and works only in a user login context. An unentitled CLI gets `errSecMissingEntitlement` (-34018) **[tested]**.
- **Prompts come from the item's ACL, which records the creating binary's code signing *designated requirement* (DR), not its path.**
  - `cargo build`, `cargo install`, and Homebrew on Apple Silicon all produce ad-hoc signed binaries, and an ad-hoc DR is `cdhash H"…"`.
  - So every rebuild or `brew upgrade` fails the ACL. In the GUI that means an "allow access" prompt. With user interaction off it fails with `errSecAuthFailed` (-25293) **[tested]**.
  - A byte-identical rebuild keeps the same cdhash and still reads silently **[tested]**.
- **A stable signing certificate removes the rebuild prompt.**
  - Signed with a certificate, the DR becomes `identifier "<id>" and certificate leaf = H"<cert hash>"`.
  - A later build signed with the same certificate read the item silently. This held even for a local self-signed certificate **[tested]**.
  - Options for Giles: release binaries signed by the project (Developer ID), or a signing step the operator runs.
  - Homebrew re-applies an **ad-hoc** signature on arm64 to every binary it patches during relocation ([`keg.rb`](https://github.com/Homebrew/brew/blob/HEAD/Library/Homebrew/extend/os/mac/keg.rb)). A Homebrew formula therefore wouldn't keep a stable DR for any binary it patches.
- **Escape hatches that avoid prompts also weaken isolation.**
  - `security add-generic-password -A` lets any app read the item.
  - An item created by `security add-generic-password` *without* `-T` trusts `/usr/bin/security`. Any process of that user can then read it with `security find-generic-password -w`, with no prompt **[tested]**.
- **Boot and SSH.**
  - Before GUI login, LaunchAgents don't run and the login keychain is locked.
  - With FileVault on, the whole data volume stays locked until someone enters a password. macOS 26 added unlocking FileVault over SSH ([apple_ssh_and_filevault(7)](https://github.com/apple-oss-distributions/OpenSSH/blob/main/apple_ssh_and_filevault.7)).
  - Over SSH without a GUI login, the login keychain is locked and no prompt can be shown. `security unlock-keychain` needs the login password ([DTS](https://developer.apple.com/forums/thread/712005)).
  - So an unattended start at boot means a root daemon with the System keychain, or a provider that doesn't use the Keychain.
- **Sandboxed plugins and MCP servers can't reach the Keychain under Giles' current Seatbelt profile.**
  - The profile allows no `mach-lookup` at all. That blocks `com.apple.SecurityServer` (securityd, the file-based keychain) and `com.apple.securityd.xpc` (secd, the data protection keychain).
  - `~/Library/Keychains` isn't readable either.
  - Denying `mach-lookup` makes items look absent (`errSecItemNotFound` -25300). Denying `com.apple.SecurityServer` alone is enough for the file-based keychain **[tested]**.
- **Disk and backup theft.**
  - The login keychain is encrypted under the user's login password, and offline cracking is possible from its hash.
  - The System keychain can be unlocked with `/var/db/SystemKey` (readable by root on a running system, and from the disk unless FileVault protects it) ([chainbreaker](https://github.com/n0fate/chainbreaker)).
  - Since macOS 26.4 a keychain file may also need a SIP-protected "protected entropy" file in `/var/db/SystemKeys`. Without it, a copied keychain file is useless ([TN3137](https://developer.apple.com/documentation/technotes/tn3137-on-mac-keychains)).
- **Rust crates.**
  - [`security-framework`](https://crates.io/crates/security-framework) 3.7.0 (Feb 2026) covers SecItem generic passwords, `SecKeychain::open`/`unlock`/`disable_user_interaction`, and `use_protected_keychain`. It has no safe API for building ACLs (trusted-app lists).
  - [`keyring`](https://crates.io/crates/keyring) 4.x is now a thin "v1"/sample layer. The real API is [`keyring-core`](https://crates.io/crates/keyring-core) 1.0.1 plus [`apple-native-keyring-store`](https://crates.io/crates/apple-native-keyring-store) 1.0.2, which is built on security-framework. Its `keychain` feature can target the User/System/Common/Dynamic keychains. Its own docs say the `protected` feature is pointless for command-line tools.

## 1. The three keychains and which contexts can use them

### File-based and data protection keychains

macOS has two keychain implementations ([TN3137](https://developer.apple.com/documentation/technotes/tn3137-on-mac-keychains)):

- **File-based keychains:** the per-user *login* keychain and the *System* keychain.
- **Data protection keychain:** iOS-style, shown as "Local Items" or "iCloud Keychain".

The SecItem API targets the file-based keychain by default. Setting `kSecUseDataProtectionKeychain` or `kSecAttrSynchronizable` targets the data protection keychain ([TN3137](https://developer.apple.com/documentation/technotes/tn3137-on-mac-keychains)).

Apple calls the file-based keychain "on the road to deprecation": `SecKeychainCreate` is deprecated in the macOS 12 SDK, and new features need the data protection keychain ([TN3137](https://developer.apple.com/documentation/technotes/tn3137-on-mac-keychains)). DTS repeats that the file-based keychain is "effectively deprecated" and that its access control "has a lot of weird legacy edge cases" ([forum 845980](https://developer.apple.com/forums/thread/845980)).

### Data protection keychain

- **Not available to daemons.** "The data protection keychain is only available in a user login context. You can't use it, for example, from a `launchd` daemon" ([TN3137](https://developer.apple.com/documentation/technotes/tn3137-on-mac-keychains)). DTS confirmed this was still true as of macOS 15 ([forum 759976](https://developer.apple.com/forums/thread/759976)).
- **Needs entitlements.** Access groups come from code-signing entitlements, which "must be authorized by a provisioning profile". That needs an app-like bundle, which is "not standard for command-line tools". A wrapped CLI still works only from a user context ([TN3137](https://developer.apple.com/documentation/technotes/tn3137-on-mac-keychains)).
- **Fails for an unentitled, ad-hoc signed CLI.** `SecItemAdd` with `kSecUseDataProtectionKeychain = true` returned `-34018` (`errSecMissingEntitlement`) **[tested]**.

### File-based keychains by context

- **Search list per context.** In a user context, the search list holds the login keychain (the default) and the System keychain. In the system context it holds only the System keychain ([TN3137](https://developer.apple.com/documentation/technotes/tn3137-on-mac-keychains)).
- **System keychain is root-only for writes.** `/Library/Keychains/System.keychain` is `-rw-r--r-- root wheel`, so only root can add items. DTS recommends having the root daemon own the item and serve other processes over IPC ([forum 759976](https://developer.apple.com/forums/thread/759976)). The file has the same mode on this Mac **[tested]**.
- **Login keychain unlocks at GUI login.** The system unlocks it when the user logs in through the GUI and locks it when they log out. SSH login doesn't unlock it ([DTS, errSecInternalComponent](https://developer.apple.com/forums/thread/712005)).

### launchd contexts ([TN2083](https://developer.apple.com/library/archive/technotes/tn2083/_index.html), [launchd.plist(5)](x-man-page://5/launchd.plist))

| Program type | UID | Security context |
| --- | --- | --- |
| launchd daemon | `UserName`, default root | global, or its own per-session context with `SessionCreate` |
| GUI agent (`Aqua`, the default) | user | per-session (GUI) |
| non-GUI agent (`StandardIO`, e.g. SSH) | user | per-session |
| per-user agent (`Background`) | user | per-user |
| pre-login agent (`LoginWindow`) | root | pre-login |

The keychain relies on the security context, not only on the UID. `sudo`, or a CI runner that switches EUID but not the security context, creates a mixed context that breaks keychain access. DTS: "Running as root often causes more problems than it solves" ([forum 712005](https://developer.apple.com/forums/thread/712005)).

What this means for each way of running Giles:

- **Root LaunchDaemon:** the System keychain only. It can create and read its own items there.
- **LaunchDaemon with `UserName=<user>`:** global security context with a non-root UID. It can't write the System keychain file. The login keychain isn't in its search list and isn't reliably unlocked. One user report: a daemon with `SessionCreate` got `errSecItemNotFound`, and running it as the user got `errSecInteractionNotAllowed` ([forum 656000](https://developer.apple.com/forums/thread/656000)). Such a daemon could open a dedicated keychain file by path and unlock it with a password, but the password then has to be stored somewhere **[inference]**.
- **GUI LaunchAgent, or a terminal in a GUI session:** the login keychain, once the user is logged in.
- **`Background` agents:** a CRC bug report says moving their agent to `LimitLoadToSessionType=Background` broke login keychain access, and that the login keychain is tied to the Aqua session ([crc-org/crc#5433](https://github.com/crc-org/crc/issues/5433)). **[unverified]**: this is a secondary source, not Apple documentation.

## 2. Access control, code signatures, and prompts

### How the ACL decides

Each operation on an item has an ACL entry listing trusted apps. If the caller is in the list, access is granted. Otherwise "the system prompts the user for confirmation" with Deny, Allow, or Always Allow, and Always Allow adds the app to the entry ([Access Control Lists](https://developer.apple.com/documentation/security/access-control-lists)).

When there is no GUI, the prompt can't be shown. The result is `errSecInteractionNotAllowed`, and daemons "should always be given access to the keychain items they need beforehand" ([forum 656000](https://developer.apple.com/forums/thread/656000)).

### What gets recorded

- **The code's designated requirement (DR).** `security add-generic-password`: "By default, the application which creates an item is trusted to access its data without warning"; `-T appPath` names the trusted apps; `-A` allows any app "without warning (insecure, not recommended!)" ([security(1)](x-man-page://1/security)). In `security dump-keychain -a`, a trusted app appears as its DR. Path doesn't matter: a copy at another path with the same cdhash read the item silently **[tested]**.
- **What a DR is.** The DR is how macOS tells that two versions are "the same code" ([TN3127](https://developer.apple.com/documentation/technotes/tn3127-inside-code-signing-requirements)). Two kinds have no stable identity:
  - **Unsigned code** has no DR at all.
  - **Ad-hoc signed code** has a DR "tied to that specific version of the code". Changing the code and running it again repeats the prompt (TN3127 shows this with microphone access and names unexpected keychain alerts as a symptom).
- **Partition lists.** They were added around macOS 10.12 and are an extra ACL condition based on code signature. You need the keychain password to change one ([security(1)](x-man-page://1/security)). The partition list and the trusted-app list must *both* be satisfied, and the partition list is met if any entry matches ([forum 845980](https://developer.apple.com/forums/thread/845980)). In my experiments, generic-password items created by an ad-hoc binary, a self-signed binary, or `/usr/bin/security` in a fresh keychain showed only the three classic ACL entries, with no visible partition entry. **[unverified]**: how partition IDs are assigned to a non-Team-ID binary on current macOS. DTS treats much of this as implementation detail ([forum 847276](https://developer.apple.com/forums/thread/847276)).

### What builds produce on Apple Silicon

- **Every native executable must be signed.** "a simple ad-hoc signature is sufficient", and `clang`/`ld` sign automatically at link time ([Big Sur 11.0.1 Universal Apps release notes](https://developer.apple.com/documentation/macos-release-notes/macos-big-sur-11_0_1-universal-apps-release-notes)).
- **Observed binaries [tested].** These are all `Signature=adhoc`, with a DR of `cdhash H"…"`:
  - `target/debug/giles`: `flags=0x20002(adhoc,linker-signed)`.
  - `~/.cargo/bin/cargo`: the same.
  - `/opt/homebrew/bin/gh`: linker-signed.
  - `/opt/homebrew/bin/jq`: `flags=0x2(adhoc)`, re-signed after install.
- **Homebrew.** For each binary it patches during keg relocation, Homebrew calls `MachO.codesign!` on arm64, which applies an ad-hoc signature. On Intel it runs `codesign --sign - --force` only when the existing signature was broken ([`extend/os/mac/keg.rb`](https://github.com/Homebrew/brew/blob/HEAD/Library/Homebrew/extend/os/mac/keg.rb)). So a bottled `giles` is ad-hoc signed, and every upgrade changes its cdhash. **[inference]**: a Developer ID signature applied before bottling wouldn't survive relocation patching. A cask or a signed tarball keeps the vendor signature.

### Consequences [tested]

1. An ad-hoc `kc` binary created an item. Its ACL trusted `cdhash H"5094…"`, and reading back worked.
2. After a source change and rebuild (new cdhash), reading with user interaction disabled returned `-25293` (`errSecAuthFailed`). In a GUI session with interaction allowed, this is where the prompt appears **[inference from the ACL docs; not triggered, to avoid popping a dialog]**.
3. A byte-identical rebuild had the same cdhash, and reading succeeded.
4. I signed with a self-signed code-signing certificate (`codesign -s <hash> -i dev.giles.exp`), giving the DR `identifier "dev.giles.exp" and certificate leaf = H"d9a8…"`. A *different* build signed with the same certificate read silently. Re-signing that build ad-hoc with the same identifier failed (`-25293`).
5. An item created by `security add-generic-password` without `-T` trusted only `/usr/bin/security`. `security find-generic-password -w` printed it with no prompt, while the signed `kc` was denied.
6. With `-T <path to kc>`, the ACL recorded `kc`'s DR at that moment, and `kc` read the item silently.

### Implications for Giles [inference]

- **Ad-hoc builds.** With ad-hoc builds (`cargo install`, Homebrew), each upgrade needs either a GUI "Always Allow" or a re-provisioning step (delete and re-add the item, or `security add-generic-password -U -T <new giles>` run by the operator).
- **Daemon mode.** As a root daemon there's no GUI to click, so the item has to be re-created by the new binary or re-ACL'd by root.
- **A local self-signed certificate is not a strong identity.** If its key sits in the same user's keychain with `codesign` allowed, any process of that user can run `codesign` and sign a fake `giles` that satisfies the DR. A Developer ID certificate held off the machine doesn't have this problem.
- **Hardened runtime.** Not researched. **[unverified]** whether a same-user process can attach to an ad-hoc `giles` and read memory without it.

## 3. Boot before login, and SSH

- **Before login.**
  - LaunchDaemons run in the system context at boot.
  - GUI (Aqua) agents start "as part of the process of logging in the user" ([TN2083](https://developer.apple.com/library/archive/technotes/tn2083/_index.html)).
  - So before login, only the System keychain is usable, and only by root.
- **FileVault.**
  - With FileVault on, "the data volume is locked and unavailable during and after booting, until an account has been authenticated using a password". No third-party daemon or agent runs until then.
  - From macOS 26, a password over SSH can unlock the volume when Remote Login is on ([apple_ssh_and_filevault(7)](https://github.com/apple-oss-distributions/OpenSSH/blob/main/apple_ssh_and_filevault.7)).
  - Automatic login is disabled when FileVault is on ([Apple Support](https://support.apple.com/en-am/guide/mac-help/mchlp1158/mac); [How to log in automatically](https://support.apple.com/en-us/102316)).
- **SSH.**
  - Logging in over SSH doesn't unlock the login keychain, and the unlock dialog can't be shown. The fix is `security unlock-keychain`, which takes the keychain password ([forum 712005](https://developer.apple.com/forums/thread/712005)).
  - One forum report shows SSH sessions on Apple Silicon landing in a security context whose search list is only the System keychain ([forum 717135](https://developer.apple.com/forums/thread/717135)). **[unverified]**: how general this is.
  - **[unverified]**: whether an SSH session counts as a "user login context" for the data protection keychain. It's moot for Giles, which lacks the entitlement anyway.
- **Containers.** The container image runs Linux (in a VM on macOS) and has no access to the host's Keychain **[inference]**.

## 4. Sandboxed children (Seatbelt) and the Keychain

- **Mach services involved.** File-based keychain operations go to securityd through `com.apple.SecurityServer` (`/System/Library/LaunchDaemons/com.apple.securityd.plist`). The data protection keychain is served by `secd`, a per-user LaunchAgent, through `com.apple.securityd.xpc`, `com.apple.securityd.general`, and others (`/System/Library/LaunchAgents/com.apple.secd.plist`). macOS 27 also has `securityd_system` with `com.apple.securityd.systemkeychain` (`com.apple.securityd.system.plist`) **[observed in the plists; purpose unverified]**.
- **What Apple's App Sandbox allows.** `/System/Library/Sandbox/Profiles/application.sb` allows `mach-lookup` of `com.apple.securityd.xpc` and `com.apple.SecurityServer`, and `file-read*` on `~/Library/Keychains`. These are what a sandboxed process would need for Keychain access **[observed on this Mac]**.
- **Giles' profile already denies all of it.** Giles' generated profile has an empty `mach-lookup` allowlist and documents "no `com.apple.SecurityServer` or `trustd`". A unit test asserts that the profile contains no `mach-` rule (`Docs/technical/sandbox.md`, "Everything else is denied"; `crates/giles-core/src/sandbox/macos/profile.rs`). Home-directory reads aren't granted, so `~/Library/Keychains` is out of reach too.
- **Experiments [tested].** The probe reads an unlocked keychain that the probe itself is trusted for:
  - Under `(allow default)` plus `(deny mach-lookup (global-name "com.apple.SecurityServer"))`: unlock returned -50, and the query returned -25300 (not found).
  - Under `(deny mach-lookup)` for all services: the same result.
  - Under `(deny file-read* (literal <keychain file>))` alone: the query also failed (-25300).
  - Under a control `(allow default)`: it succeeded.
  - Failures show up as "not found", not as a sandbox error.
- **Implications [inference].**
  - Keychain access is safe from sandboxed plugins as long as the profile keeps those services and paths out. A future profile should keep `com.apple.SecurityServer`, `com.apple.securityd.*`, `com.apple.secd*`, and `~/Library/Keychains` explicitly out of any allowlist.
  - An *unsandboxed* child (a spec that opts out of the sandbox) runs as the same user. It can reach securityd and read any item whose ACL trusts `/usr/bin/security` or "any app".

## 5. Disk and backup theft

- **Login keychain.** It is encrypted under the user's keychain (login) password. chainbreaker extracts a crackable hash from it, and decrypts it given the password, a memory-derived master key, or "an unlock file such as SystemKey" ([chainbreaker README](https://github.com/n0fate/chainbreaker)).
- **System keychain.** Its unlock file is `/var/db/SystemKey` ([chainbreaker README](https://github.com/n0fate/chainbreaker)). On this Mac the file is `-r-------- root wheel` **[observed]**. Without FileVault, a disk image contains both the System keychain and its key **[inference]**.
- **macOS 26.4 and later.** A keychain file "might reference a protected entropy file" in `/var/db/SystemKeys`. That directory is "not directly accessible, even when running as root" without disabling SIP, and both files are needed to unlock the keychain ([TN3137, "Backing up a file-based keychain"](https://developer.apple.com/documentation/technotes/tn3137-on-mac-keychains)). This makes stolen keychain files and backups much less useful, where the protected entropy file is in use. **[unverified]**: which keychains get one, and whether the System keychain does.

## 6. Rust crates

- **security-framework** ([crates.io](https://crates.io/crates/security-framework): 3.7.0, Feb 2026, kornelski, about 424M downloads):
  - `passwords::{set_generic_password, generic_password, delete_generic_password}`.
  - `PasswordOptions` with `use_protected_keychain()` ("always true except on macOS"), `set_access_group`, `set_access_control`, and `set_access_synchronized` ([docs](https://docs.rs/security-framework/latest/security_framework/passwords/struct.PasswordOptions.html)).
  - `os::macos::keychain::SecKeychain` with `open(path)`, `default_for_domain`, `unlock`, `disable_user_interaction` (an RAII lock), and `find_generic_password` ([docs](https://docs.rs/security-framework/latest/security_framework/os/macos/keychain/struct.SecKeychain.html)).
  - `os::macos::access::SecAccess` is only the type: there are no bindings to create ACLs or trusted applications (`SecAccessCreate`, `SecTrustedApplicationCreateFromPath`). I grepped the source for them **[tested]**.
  - So "trust exactly this binary" or "trust these apps" ACLs would need hand-written FFI to deprecated APIs, or the operator running `security add-generic-password -T`. The default creator-trusted ACL works without any of that.
- **keyring**: [4.2.0](https://crates.io/crates/keyring) (Aug 2026, open-source-cooperative).
  - The crate now offers a `v1` mode (one-call `Entry`, with the macOS backend being "Keychain Services") and a `cli` sample mode.
  - It says applications that need control should depend on [`keyring-core`](https://crates.io/crates/keyring-core) (1.0.1, Oct 2026) and specific stores instead ([docs](https://docs.rs/keyring/latest/keyring/)).
- **apple-native-keyring-store** ([1.0.2](https://crates.io/crates/apple-native-keyring-store), Aug 2026) is the macOS store for keyring-core, built on security-framework.
  - Feature `keychain`: the file-based store. The default is the User (login) keychain, and the configuration can pick `User`, `System`, `Common`, or `Dynamic` ([docs](https://docs.rs/apple-native-keyring-store/latest/apple_native_keyring_store/keychain/index.html)).
  - Feature `protected`: the data protection store. Its docs say "command-line tools cannot be code-signed [with a provisioning profile] … there's not much point in their using this module" ([docs](https://docs.rs/apple-native-keyring-store/latest/apple_native_keyring_store/protected/index.html)).
  - Neither exposes ACL or trusted-app control **[inference from the documented API]**.

## Experiments run on this Mac

All on macOS 27.0 (26A428), arm64, using a throwaway keychain in `/tmp` that was never added to the search list (the search list was restored and checked afterwards). The probe was a small C program built with `clang -framework Security`. It opened and unlocked the temporary keychain, called `SecKeychainSetUserInteractionAllowed(false)` so nothing could prompt, and then ran `SecItemAdd` and `SecItemCopyMatching` with `kSecUseKeychain` and `kSecMatchSearchList`. `codesign -d -r-` printed the DRs, and `security dump-keychain -a` printed the ACLs. Everything was deleted afterwards. The real login keychain wasn't touched, except for one `SecItemAdd` to the data protection keychain, which was rejected with -34018 and so created nothing.

| Case | Result |
| --- | --- |
| Ad-hoc probe creates the item, then reads it | `add 0`, `get 0` |
| Rebuild with changed source (new cdhash) | `get -25293` |
| Byte-identical rebuild, including at another path | `get 0` |
| Self-signed cert build `s1` creates, cert build `s2` reads | `get 0` |
| Same `s2` code re-signed ad-hoc | `get -25293` |
| `security add-generic-password` without `-T`, read by `security -w` | value printed, no prompt |
| Same item read by the signed probe | `get -25293` |
| `security add-generic-password -T <probe>`, read by the probe | `get 0` |
| `security … -A` item, read by the probe | `get 0` |
| Seatbelt: deny `mach-lookup` for `com.apple.SecurityServer` | unlock -50, `get -25300` |
| Seatbelt: deny all `mach-lookup` | unlock -50, `get -25300` |
| Seatbelt: deny `file-read*` on the keychain file | `get -25300` |
| Data protection `SecItemAdd` from the unentitled ad-hoc CLI | `-34018` |
