//! Tests of the SSH keys service and of reading every upload format. The upload fixtures in
//! `testdata/` are checked in; `testdata/generate.sh` says how they were made.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use doozers_store::{Role, SshKeyId, UserState};
use ssh_key::private::{KeypairData, SkEd25519};
use ssh_key::public::{Ed25519PublicKey, SkEd25519 as SkEd25519Public};
use ssh_key::{HashAlg, LineEnding, PublicKey, SshSig};

use super::formats::{KeyFileError, MAX_UPLOAD_BYTES, parse_private_key};
use super::*;
use crate::identity;
use crate::paths::DataRoot;
use crate::secrets::{KeyFile, LocalSecrets, SecretMetadata};
use crate::users::User;

/// The passphrase of every encrypted fixture.
const PASS: &str = "correct horse battery";

fn fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/ssh/testdata")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn fixture_text(name: &str) -> String {
    String::from_utf8(fixture(name)).unwrap()
}

/// The fingerprint of the public key in the fixture `name`.
fn fixture_fingerprint(name: &str) -> String {
    PublicKey::from_openssh(&fixture_text(name))
        .unwrap()
        .fingerprint(HashAlg::Sha256)
        .to_string()
}

fn user(name: &str) -> UserId {
    UserId::new(name).unwrap()
}

fn owner() -> UserId {
    identity::owner()
}

/// Whether `ssh-keygen` runs here. Tests that need it print why they stop when it doesn't.
fn have_ssh_keygen(test: &str) -> bool {
    let found = Command::new("ssh-keygen")
        .arg("-?")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok();
    if !found {
        eprintln!("skipping {test}: ssh-keygen isn't installed");
    }
    found
}

/// `ssh-keygen -lf` of a public key line: its fingerprint.
fn keygen_fingerprint(dir: &Path, public_key: &str) -> String {
    let file = dir.join("key.pub");
    std::fs::write(&file, format!("{public_key}\n")).unwrap();
    let out = Command::new("ssh-keygen")
        .args(["-l", "-f"])
        .arg(&file)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_owned()
}

/// The SSH signature (namespace `file`, SHA-512) of `message` by `key`, made the way the signing
/// path makes it.
fn ssh_sig(key: &PrivateKey, message: &[u8]) -> SshSig {
    let data = SshSig::signed_data("file", HashAlg::Sha512, message).unwrap();
    let signature = keys::sign(key, HashAlg::Sha512, &data).unwrap();
    SshSig::new(
        key.public_key().key_data().clone(),
        "file",
        HashAlg::Sha512,
        signature,
    )
    .unwrap()
}

/// Signs `message` with `key` (namespace `file`) and has `ssh-keygen -Y verify` check it against
/// `public_key`. Returns whether it verified.
fn keygen_verifies(dir: &Path, key: &PrivateKey, public_key: &str, message: &[u8]) -> bool {
    let signature = ssh_sig(key, message);
    let sig_file = dir.join("message.sig");
    std::fs::write(&sig_file, signature.to_pem(LineEnding::LF).unwrap()).unwrap();
    let allowed = dir.join("allowed_signers");
    std::fs::write(
        &allowed,
        format!("owner@doozers namespaces=\"file\" {public_key}\n"),
    )
    .unwrap();
    let mut child = Command::new("ssh-keygen")
        .args(["-Y", "verify", "-I", "owner@doozers", "-n", "file", "-f"])
        .arg(&allowed)
        .arg("-s")
        .arg(&sig_file)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(message).unwrap();
    child.wait().unwrap().success()
}

// ---- reading upload formats ----

/// Every fixture format: (file, public key file of the same key, passphrase, algorithm).
fn upload_cases() -> Vec<(String, String, bool, &'static str)> {
    let mut cases: Vec<(String, String, bool, &'static str)> = Vec::new();
    let mut add = |file: &str, public: &str, encrypted: bool, algorithm: &'static str| {
        cases.push((file.to_owned(), public.to_owned(), encrypted, algorithm));
    };
    add("ed25519", "ed25519.pub", false, "ssh-ed25519");
    add(
        "ed25519-encrypted",
        "ed25519-encrypted.pub",
        true,
        "ssh-ed25519",
    );
    add(
        "ed25519-pkcs8.pem",
        "ed25519-pkcs8.pub",
        false,
        "ssh-ed25519",
    );
    add(
        "ed25519-pkcs8-encrypted.pem",
        "ed25519-pkcs8.pub",
        true,
        "ssh-ed25519",
    );
    add(
        "ed25519-pkcs8-scrypt.pem",
        "ed25519-pkcs8.pub",
        true,
        "ssh-ed25519",
    );
    add("rsa", "rsa.pub", false, "ssh-rsa");
    add("rsa-encrypted", "rsa-encrypted.pub", true, "ssh-rsa");
    add("rsa-pkcs1", "rsa.pub", false, "ssh-rsa");
    add("rsa-pkcs1-encrypted", "rsa-encrypted.pub", true, "ssh-rsa");
    add("rsa-pkcs1-aes192", "rsa.pub", true, "ssh-rsa");
    add("rsa-pkcs1-aes256", "rsa.pub", true, "ssh-rsa");
    add("rsa-pkcs8.pem", "rsa.pub", false, "ssh-rsa");
    add("rsa-pkcs8-encrypted.pem", "rsa.pub", true, "ssh-rsa");
    for (bits, algorithm) in [
        ("p256", "ecdsa-sha2-nistp256"),
        ("p384", "ecdsa-sha2-nistp384"),
        ("p521", "ecdsa-sha2-nistp521"),
    ] {
        let openssh = format!("ecdsa-{bits}");
        let sec1 = format!("ec-{bits}-sec1");
        let pkcs8 = format!("ec-{bits}-pkcs8");
        add(&openssh, &format!("{openssh}.pub"), false, algorithm);
        add(
            &format!("{openssh}-encrypted"),
            &format!("{openssh}-encrypted.pub"),
            true,
            algorithm,
        );
        add(&sec1, &format!("{openssh}.pub"), false, algorithm);
        add(
            &format!("{sec1}-encrypted"),
            &format!("{openssh}-encrypted.pub"),
            true,
            algorithm,
        );
        add(
            &format!("{pkcs8}.pem"),
            &format!("{openssh}.pub"),
            false,
            algorithm,
        );
        add(
            &format!("{pkcs8}-encrypted.pem"),
            &format!("{openssh}.pub"),
            true,
            algorithm,
        );
    }
    for bits in ["p256", "p384", "p521"] {
        let algorithm = match bits {
            "p256" => "ecdsa-sha2-nistp256",
            "p384" => "ecdsa-sha2-nistp384",
            _ => "ecdsa-sha2-nistp521",
        };
        let public = format!("ecdsa-{bits}.pub");
        add(&format!("ec-{bits}-sec1-named"), &public, false, algorithm);
        add(
            &format!("ec-{bits}-sec1-named-encrypted"),
            &public,
            true,
            algorithm,
        );
    }
    add(
        "ec-p256-sec1-aes256",
        "ecdsa-p256.pub",
        true,
        "ecdsa-sha2-nistp256",
    );
    cases
}

#[test]
fn every_upload_format_reads_back_as_the_same_key() {
    for (file, public, encrypted, algorithm) in upload_cases() {
        let key = parse_private_key(&fixture(&file), encrypted.then_some(PASS))
            .unwrap_or_else(|e| panic!("{file}: {e}"));
        let parts = keys::public_parts(&key).unwrap();
        assert_eq!(parts.algorithm, algorithm, "{file}");
        assert_eq!(parts.fingerprint, fixture_fingerprint(&public), "{file}");
        assert!(!key.is_encrypted(), "{file}");
    }
}

#[test]
fn an_openssh_key_keeps_its_comment_and_the_others_have_none() {
    let key = parse_private_key(&fixture("rsa"), None).unwrap();
    assert_eq!(key.comment(), "rsa@fixture");
    let key = parse_private_key(&fixture("rsa-pkcs1"), None).unwrap();
    assert_eq!(key.comment(), "");
}

#[test]
fn encrypted_uploads_need_the_right_passphrase() {
    for (file, _, encrypted, _) in upload_cases() {
        if !encrypted {
            continue;
        }
        let bytes = fixture(&file);
        assert_eq!(
            parse_private_key(&bytes, None).unwrap_err(),
            KeyFileError::PassphraseRequired,
            "{file}: none"
        );
        assert_eq!(
            parse_private_key(&bytes, Some("")).unwrap_err(),
            KeyFileError::PassphraseRequired,
            "{file}: empty"
        );
        assert_eq!(
            parse_private_key(&bytes, Some("not the passphrase")).unwrap_err(),
            KeyFileError::WrongPassphrase,
            "{file}: wrong"
        );
        // A near miss is wrong too.
        assert_eq!(
            parse_private_key(&bytes, Some(&format!("{PASS} "))).unwrap_err(),
            KeyFileError::WrongPassphrase,
            "{file}: trailing space"
        );
    }
}

#[test]
fn a_passphrase_for_an_unencrypted_key_is_ignored() {
    for (file, _, encrypted, _) in upload_cases() {
        if encrypted {
            continue;
        }
        parse_private_key(&fixture(&file), Some("unneeded"))
            .unwrap_or_else(|e| panic!("{file}: {e}"));
    }
}

#[test]
fn pasted_text_survives_a_browsers_line_endings_and_padding() {
    let text = fixture_text("ecdsa-p256-encrypted");
    let crlf = format!("\n  {}  \n\n", text.replace('\n', "\r\n"));
    let key = parse_private_key(crlf.as_bytes(), Some(PASS)).unwrap();
    assert_eq!(
        keys::public_parts(&key).unwrap().fingerprint,
        fixture_fingerprint("ecdsa-p256-encrypted.pub")
    );
    // Legacy encrypted PEM has headers, which a textarea breaks the same way.
    let text = fixture_text("rsa-pkcs1-encrypted").replace('\n', "\r\n");
    parse_private_key(text.as_bytes(), Some(PASS)).unwrap();
}

#[test]
fn putty_dsa_and_security_keys_are_refused_with_their_reasons() {
    let err = parse_private_key(&fixture("putty.ppk"), None).unwrap_err();
    assert_eq!(err, KeyFileError::Putty);
    assert!(
        err.to_string()
            .contains("puttygen key.ppk -O private-openssh -o key")
    );

    for file in ["dsa-traditional", "dsa-pkcs8.pem", "dsa-openssh"] {
        let err = parse_private_key(&fixture(file), None).unwrap_err();
        assert_eq!(err, KeyFileError::Dsa, "{file}");
        assert!(err.to_string().contains("OpenSSH 10.0 removed DSA"));
    }

    let sk = SkEd25519::new(
        SkEd25519Public::new(Ed25519PublicKey([7; 32]), "ssh:"),
        0x01,
        [1u8; 16],
    )
    .map(KeypairData::from)
    .unwrap();
    let pem = PrivateKey::new(sk, "token")
        .unwrap()
        .to_openssh(LineEnding::LF)
        .unwrap();
    let err = parse_private_key(pem.as_bytes(), None).unwrap_err();
    assert_eq!(err, KeyFileError::SecurityKey);
    assert!(err.to_string().contains("hardware"));
}

#[test]
fn other_algorithms_and_curves_are_refused() {
    for file in [
        "ed448-pkcs8.pem",
        "ec-secp256k1-sec1",
        "ec-secp256k1-explicit-sec1",
    ] {
        let err = parse_private_key(&fixture(file), None).unwrap_err();
        assert!(
            matches!(err, KeyFileError::UnsupportedAlgorithm(_)),
            "{file}: {err:?}"
        );
        assert!(
            err.to_string()
                .contains("isn't supported; use Ed25519, RSA, or ECDSA"),
            "{err}"
        );
    }
}

#[test]
fn an_unreadable_cipher_says_how_to_convert() {
    let text = fixture_text("rsa-pkcs1-encrypted");
    let swapped: String = text
        .lines()
        .map(|line| {
            if line.starts_with("DEK-Info:") {
                "DEK-Info: DES-EDE3-CBC,8790036DFB5A16E2".to_owned()
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let err = parse_private_key(swapped.as_bytes(), Some(PASS)).unwrap_err();
    assert!(matches!(&err, KeyFileError::UnsupportedCipher(name) if name == "DES-EDE3-CBC"));
    assert!(err.to_string().contains("ssh-keygen -p -f <file>"));
}

#[test]
fn encryption_settings_that_would_exhaust_the_server_are_refused_before_decrypting() {
    // Each file asks for more than any real key needs: scrypt with N = 2^40 (hundreds of
    // terabytes), r = 16, or p = 2, PBKDF2 with 99,999,999 iterations, and OpenSSH's bcrypt with
    // 4,294,967,295 rounds. Decrypting any of them would abort, run out of memory, or take
    // minutes, so the test passing at all shows they are refused first.
    for (file, setting) in [
        ("pkcs8-scrypt-n.pem", "scrypt N"),
        ("pkcs8-scrypt-r.pem", "scrypt r"),
        ("pkcs8-scrypt-p.pem", "scrypt p"),
        ("pkcs8-pbkdf2-iterations.pem", "PBKDF2 iteration count"),
        ("ed25519-bcrypt-rounds", "bcrypt rounds"),
    ] {
        let err = parse_private_key(&fixture(file), Some(PASS)).unwrap_err();
        assert!(
            matches!(&err, KeyFileError::TooExpensive(what) if what.contains(setting)),
            "{file}: {err:?}"
        );
        assert!(err.to_string().contains("ssh-keygen -p -f <file>"), "{err}");
    }
}

#[test]
fn things_that_are_not_private_keys_are_refused() {
    let public = fixture("ed25519.pub");
    assert_eq!(
        parse_private_key(&public, None).unwrap_err(),
        KeyFileError::PublicKey
    );
    assert_eq!(
        parse_private_key(b"", None).unwrap_err(),
        KeyFileError::Empty
    );
    assert_eq!(
        parse_private_key(b"  \r\n ", None).unwrap_err(),
        KeyFileError::Empty
    );
    assert_eq!(
        parse_private_key(b"hello", None).unwrap_err(),
        KeyFileError::NotAKey
    );
    assert_eq!(
        parse_private_key(&[0xff, 0xfe, 0x00], None).unwrap_err(),
        KeyFileError::NotAKey
    );
    let unterminated = fixture_text("rsa").replace("-----END OPENSSH PRIVATE KEY-----", "");
    assert_eq!(
        parse_private_key(unterminated.as_bytes(), None).unwrap_err(),
        KeyFileError::NotAKey
    );
    let certificate = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";
    assert_eq!(
        parse_private_key(certificate.as_bytes(), None).unwrap_err(),
        KeyFileError::UnsupportedFormat("CERTIFICATE".to_owned())
    );
    let big = vec![b'a'; MAX_UPLOAD_BYTES + 1];
    assert_eq!(
        parse_private_key(&big, None).unwrap_err(),
        KeyFileError::TooLarge
    );
}

#[test]
fn damaged_keys_are_reported_without_panicking() {
    for file in [
        "rsa",
        "rsa-pkcs1",
        "ec-p256-sec1",
        "ed25519-pkcs8.pem",
        "ecdsa-p384",
    ] {
        // Cut the last eight characters off the first line of the body: still base64, but short.
        let text = fixture_text(file);
        let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
        let first = &mut lines[1];
        first.truncate(first.len() - 8);
        let err = parse_private_key(lines.join("\n").as_bytes(), None).unwrap_err();
        assert!(matches!(err, KeyFileError::Malformed(_)), "{file}: {err:?}");
    }
    let garbage = "-----BEGIN RSA PRIVATE KEY-----\n!!!!\n-----END RSA PRIVATE KEY-----";
    assert!(matches!(
        parse_private_key(garbage.as_bytes(), None).unwrap_err(),
        KeyFileError::Malformed(_)
    ));
}

// ---- the service ----

#[derive(Debug)]
struct TestClock(Mutex<Timestamp>);

impl TestClock {
    fn set(&self, seconds: i64) {
        *self.0.lock().unwrap() = Timestamp::from_second(1_800_000_000 + seconds).unwrap();
    }
}

impl Clock for TestClock {
    fn now(&self) -> Timestamp {
        *self.0.lock().unwrap()
    }
}

/// A secret store that fails on demand, in front of the real one.
#[derive(Debug)]
struct Flaky {
    inner: LocalSecrets,
    fail_put: AtomicBool,
    fail_delete: AtomicBool,
}

#[async_trait]
impl SecretProvider for Flaky {
    async fn get(&self, path: &SecretPath) -> Result<Option<Secret>, SecretError> {
        self.inner.get(path).await
    }

    async fn metadata(&self, path: &SecretPath) -> Result<Option<SecretMetadata>, SecretError> {
        self.inner.metadata(path).await
    }

    async fn list(&self, under: Option<&SecretPath>) -> Result<Vec<SecretMetadata>, SecretError> {
        self.inner.list(under).await
    }

    async fn put(
        &self,
        path: &SecretPath,
        value: &Secret,
        by: Option<&UserId>,
    ) -> Result<SecretMetadata, SecretError> {
        if self.fail_put.load(Ordering::SeqCst) {
            return Err(SecretError::Unavailable("down".into()));
        }
        self.inner.put(path, value, by).await
    }

    async fn describe(
        &self,
        path: &SecretPath,
        description: &str,
        by: Option<&UserId>,
    ) -> Result<SecretMetadata, SecretError> {
        self.inner.describe(path, description, by).await
    }

    async fn delete(&self, path: &SecretPath) -> Result<bool, SecretError> {
        if self.fail_delete.load(Ordering::SeqCst) {
            return Err(SecretError::Unavailable("down".into()));
        }
        self.inner.delete(path).await
    }
}

struct Env {
    dir: tempfile::TempDir,
    store: Store,
    secrets: Arc<Flaky>,
    clock: Arc<TestClock>,
    keys: SshKeys,
    /// The users the service asks: `owner` and `admin2` are admins, `alice` and `bob` members.
    users: UserList,
}

impl Env {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::new(dir.path(), Path::new("/tmp")).unwrap();
        let store = Store::open(dir.path().join("doozers.db")).await.unwrap();
        let clock = Arc::new(TestClock(Mutex::new(
            Timestamp::from_second(1_800_000_000).unwrap(),
        )));
        let inner = LocalSecrets::open(
            store.clone(),
            &KeyFile::new(&root, &root.secret_key_file()),
            clock.clone(),
        )
        .await
        .unwrap();
        let secrets = Arc::new(Flaky {
            inner,
            fail_put: AtomicBool::new(false),
            fail_delete: AtomicBool::new(false),
        });
        let users = UserList::with_roles(&["owner", "admin2"], &["alice", "bob"]);
        let keys = SshKeys::new(store.clone(), secrets.clone(), clock.clone(), users.clone());
        Self {
            dir,
            store,
            secrets,
            clock,
            keys,
            users,
        }
    }

    /// The paths of every secret in the store.
    async fn secret_paths(&self) -> Vec<String> {
        self.secrets
            .list(None)
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.path.to_string())
            .collect()
    }

    /// The stored private key text of a key.
    async fn stored_pem(&self, user: &UserId, id: SshKeyId) -> Option<String> {
        let path = SecretPath::new(format!("users/{user}/ssh-keys/{id}")).unwrap();
        self.secrets
            .get(&path)
            .await
            .unwrap()
            .map(|secret| secret.expose().to_owned())
    }

    async fn upload(&self, file: &str, passphrase: Option<&str>) -> SshKey {
        self.keys
            .upload(&owner(), &fixture(file), passphrase, None)
            .await
            .unwrap_or_else(|e| panic!("{file}: {e}"))
    }
}

#[tokio::test]
async fn every_type_generates_and_its_fingerprint_matches_ssh_keygen() {
    let env = Env::new().await;
    let ssh_keygen = have_ssh_keygen("the ssh-keygen fingerprint comparison");
    let expected = [
        (KeyType::Ed25519, "ssh-ed25519", "Ed25519"),
        (KeyType::Rsa3072, "ssh-rsa", "RSA 3072"),
        (KeyType::Rsa4096, "ssh-rsa", "RSA 4096"),
        (KeyType::EcdsaP256, "ecdsa-sha2-nistp256", "ECDSA P-256"),
        (KeyType::EcdsaP384, "ecdsa-sha2-nistp384", "ECDSA P-384"),
        (KeyType::EcdsaP521, "ecdsa-sha2-nistp521", "ECDSA P-521"),
    ];
    assert_eq!(KeyType::ALL.len(), expected.len());
    for (kind, algorithm, label) in expected {
        let key = env.keys.generate(&owner(), kind, None).await.unwrap();
        assert_eq!(key.algorithm, algorithm, "{kind:?}");
        assert_eq!(key.type_label, label, "{kind:?}");
        assert_eq!(key.type_label, kind.label());
        assert_eq!(key.label, "owner@doozers");
        assert!(
            key.public_key.starts_with(&format!("{algorithm} AAAA")),
            "{}",
            key.public_key
        );
        assert!(!key.public_key.contains(' ') || key.public_key.matches(' ').count() == 1);
        assert!(key.fingerprint.starts_with("SHA256:"));
        assert_eq!(key.last_used_at, None);
        assert_eq!(
            key.authorized_key(),
            format!("{} owner@doozers", key.public_key)
        );
        if ssh_keygen {
            assert_eq!(
                keygen_fingerprint(env.dir.path(), &key.public_key),
                key.fingerprint,
                "{kind:?}"
            );
        }
        assert_eq!(KeyType::from_id(kind.id()), Some(kind));
    }
    assert_eq!(KeyType::from_id("dsa"), None);
    assert_eq!(KeyType::default(), KeyType::Ed25519);
}

#[tokio::test]
async fn a_generated_key_is_stored_as_unencrypted_openssh_text_without_a_comment() {
    let env = Env::new().await;
    let key = env
        .keys
        .generate(&owner(), KeyType::Ed25519, Some("  laptop "))
        .await
        .unwrap();
    assert_eq!(key.label, "laptop");
    assert_eq!(
        env.secret_paths().await,
        [format!("users/owner/ssh-keys/{}", key.id)]
    );
    let pem = env.stored_pem(&owner(), key.id).await.unwrap();
    assert!(
        pem.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----\n"),
        "{pem}"
    );
    let parsed = PrivateKey::from_openssh(&pem).unwrap();
    assert!(!parsed.is_encrypted());
    assert_eq!(parsed.comment(), "");
    assert_eq!(
        parsed.fingerprint(HashAlg::Sha256).to_string(),
        key.fingerprint
    );
    // The secret is the user's own: written by them.
    let meta = env
        .secrets
        .metadata(&SecretPath::new(format!("users/owner/ssh-keys/{}", key.id)).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(meta.created_by, Some(owner()));
}

#[tokio::test]
async fn a_signature_by_a_key_read_back_from_the_secret_store_verifies_with_ssh_keygen() {
    if !have_ssh_keygen("the ssh-keygen -Y verify test") {
        return;
    }
    let env = Env::new().await;
    let message = b"the doozers signature test";
    let mut keys = Vec::new();
    for kind in KeyType::ALL {
        keys.push(
            env.keys
                .generate(&owner(), kind, Some(kind.id()))
                .await
                .unwrap(),
        );
    }
    // Uploaded keys too: one of each family through a non-OpenSSH format.
    for file in [
        "rsa-pkcs1",
        "ec-p384-pkcs8.pem",
        "ed25519-pkcs8-encrypted.pem",
    ] {
        keys.push(env.upload(file, Some(PASS)).await);
    }
    for key in keys {
        let private = env.keys.private_key(&owner(), key.id).await.unwrap();
        assert!(
            keygen_verifies(env.dir.path(), &private, &key.public_key, message),
            "{}",
            key.label
        );
        // A different message doesn't verify against that signature.
        let signature = ssh_sig(&private, message);
        let public = PublicKey::from_openssh(&key.public_key).unwrap();
        assert!(
            public
                .verify("file", b"another message", &signature)
                .is_err()
        );
        assert!(public.verify("file", message, &signature).is_ok());
    }
}

#[tokio::test]
async fn every_upload_format_is_added_with_the_right_fingerprint_and_label() {
    let env = Env::new().await;
    for (n, (file, public, encrypted, algorithm)) in upload_cases().into_iter().enumerate() {
        // A fresh user each time, so the same key in two formats doesn't clash.
        let user = user(&format!("u{n}"));
        let key = env
            .keys
            .upload(&user, &fixture(&file), encrypted.then_some(PASS), None)
            .await
            .unwrap_or_else(|e| panic!("{file}: {e}"));
        assert_eq!(key.algorithm, algorithm, "{file}");
        assert_eq!(key.fingerprint, fixture_fingerprint(&public), "{file}");
        let stored =
            PrivateKey::from_openssh(env.stored_pem(&user, key.id).await.unwrap()).unwrap();
        assert!(
            !stored.is_encrypted(),
            "{file}: the passphrase isn't stored"
        );
        assert_eq!(
            stored.fingerprint(HashAlg::Sha256).to_string(),
            key.fingerprint,
            "{file}"
        );
        // The label is the key's comment, or `<user>@doozers` when it has none.
        let openssh = !file.contains("pkcs") && !file.contains("sec1") && !file.contains("aes");
        let comment = file.trim_end_matches("-encrypted");
        let comment = if file.starts_with("ed25519-pkcs8") {
            "none"
        } else {
            comment
        };
        let expected = if openssh && !comment.is_empty() {
            format!("{comment}@fixture")
        } else {
            format!("{user}@doozers")
        };
        assert_eq!(key.label, expected, "{file}");
    }
}

#[tokio::test]
async fn an_uploaded_labels_can_be_chosen() {
    let env = Env::new().await;
    let key = env
        .keys
        .upload(&owner(), &fixture("rsa"), None, Some("work laptop"))
        .await
        .unwrap();
    assert_eq!(key.label, "work laptop");
    // An unusable comment falls back to the default rather than refusing the key.
    let long_comment = "c".repeat(150);
    let pem = PrivateKey::from_openssh(fixture_text("ecdsa-p256")).unwrap();
    let mut pem = pem;
    pem.set_comment(long_comment);
    let key = env
        .keys
        .upload(
            &owner(),
            pem.to_openssh(LineEnding::LF).unwrap().as_bytes(),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(key.label, "owner@doozers");
}

#[tokio::test]
async fn a_wrong_or_missing_passphrase_adds_nothing() {
    let env = Env::new().await;
    for file in [
        "ecdsa-p256-encrypted",
        "rsa-pkcs1-encrypted",
        "ec-p384-pkcs8-encrypted.pem",
    ] {
        let err = env
            .keys
            .upload(&owner(), &fixture(file), Some("wrong"), None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, SshKeysError::Upload(KeyFileError::WrongPassphrase)),
            "{file}: {err:?}"
        );
        assert_eq!(err.to_string(), "the passphrase is wrong");
        let err = env
            .keys
            .upload(&owner(), &fixture(file), None, None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, SshKeysError::Upload(KeyFileError::PassphraseRequired)),
            "{file}: {err:?}"
        );
    }
    assert!(env.keys.list(&owner()).await.unwrap().is_empty());
    assert!(env.secret_paths().await.is_empty());
    // The right one works.
    env.upload("ecdsa-p256-encrypted", Some(PASS)).await;
    assert_eq!(env.keys.list(&owner()).await.unwrap().len(), 1);
}

#[tokio::test]
async fn refused_uploads_leave_nothing_behind() {
    let env = Env::new().await;
    for file in [
        "putty.ppk",
        "dsa-traditional",
        "ed448-pkcs8.pem",
        "ed25519.pub",
    ] {
        let err = env
            .keys
            .upload(&owner(), &fixture(file), None, None)
            .await
            .unwrap_err();
        assert!(matches!(err, SshKeysError::Upload(_)), "{file}: {err:?}");
    }
    assert!(env.keys.list(&owner()).await.unwrap().is_empty());
    assert!(env.secret_paths().await.is_empty());
}

#[tokio::test]
async fn a_user_cant_add_a_key_twice_but_two_users_can_share_one() {
    let env = Env::new().await;
    let first = env
        .keys
        .upload(&owner(), &fixture("rsa"), None, Some("first"))
        .await
        .unwrap();
    // The same key in another format is the same key.
    let err = env
        .keys
        .upload(&owner(), &fixture("rsa-pkcs8.pem"), None, Some("second"))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, SshKeysError::Duplicate { label } if label == "first"),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "you already have this key, labelled \"first\""
    );
    assert_eq!(env.keys.list(&owner()).await.unwrap().len(), 1);
    assert_eq!(
        env.secret_paths().await.len(),
        1,
        "no secret for the refused upload"
    );

    // Another user gets their own row and their own secret for the same private key.
    let alice = user("alice");
    let copy = env
        .keys
        .upload(&alice, &fixture("rsa"), None, None)
        .await
        .unwrap();
    assert_ne!(copy.id, first.id);
    assert_eq!(copy.fingerprint, first.fingerprint);
    assert_eq!(env.secret_paths().await.len(), 2);
    let a = env.stored_pem(&owner(), first.id).await.unwrap();
    let b = env.stored_pem(&alice, copy.id).await.unwrap();
    assert_eq!(a, b);
    // Deleting one leaves the other.
    env.keys.delete(&alice, copy.id).await.unwrap();
    assert!(env.stored_pem(&owner(), first.id).await.is_some());
    assert_eq!(env.keys.list(&owner()).await.unwrap().len(), 1);
}

#[tokio::test]
async fn labels_are_trimmed_and_checked() {
    let env = Env::new().await;
    let key = env
        .keys
        .generate(&owner(), KeyType::Ed25519, Some("   "))
        .await
        .unwrap();
    assert_eq!(key.label, "owner@doozers", "a blank label is the default");
    let hundred = "é".repeat(100);
    let key = env
        .keys
        .rename(&owner(), key.id, &format!("  {hundred}  "))
        .await
        .unwrap();
    assert_eq!(key.label, hundred, "100 characters, not bytes");
    for bad in [
        "",
        "   ",
        &"x".repeat(101),
        "two\nlines",
        "tab\there",
        "bell\u{7}",
    ] {
        let err = env.keys.rename(&owner(), key.id, bad).await.unwrap_err();
        assert!(
            matches!(err, SshKeysError::InvalidLabel(_)),
            "{bad:?}: {err:?}"
        );
        let err = env
            .keys
            .generate(&owner(), KeyType::Ed25519, Some(bad))
            .await
            .map(|_| ());
        // Blank is the default for a new key; anything else bad is refused.
        if bad.trim().is_empty() {
            assert!(err.is_ok());
        } else {
            assert!(matches!(err, Err(SshKeysError::InvalidLabel(_))), "{bad:?}");
        }
    }
    assert_eq!(env.keys.get(&owner(), key.id).await.unwrap().label, hundred);
    // Labels needn't be unique.
    let other = env
        .keys
        .generate(&owner(), KeyType::Ed25519, Some("same"))
        .await
        .unwrap();
    let again = env
        .keys
        .generate(&owner(), KeyType::Ed25519, Some("same"))
        .await
        .unwrap();
    assert_eq!(other.label, again.label);
}

#[tokio::test]
async fn rename_changes_only_the_label() {
    let env = Env::new().await;
    let key = env
        .keys
        .generate(&owner(), KeyType::EcdsaP256, Some("old"))
        .await
        .unwrap();
    let renamed = env.keys.rename(&owner(), key.id, "new").await.unwrap();
    assert_eq!(
        renamed,
        SshKey {
            label: "new".to_owned(),
            ..key.clone()
        }
    );
    assert_eq!(renamed.authorized_key(), format!("{} new", key.public_key));
    // The stored secret is untouched.
    let pem = PrivateKey::from_openssh(env.stored_pem(&owner(), key.id).await.unwrap()).unwrap();
    assert_eq!(
        pem.fingerprint(HashAlg::Sha256).to_string(),
        key.fingerprint
    );
}

#[tokio::test]
async fn delete_removes_the_row_and_the_secret_and_ids_are_never_reused() {
    let env = Env::new().await;
    let first = env
        .keys
        .generate(&owner(), KeyType::Ed25519, None)
        .await
        .unwrap();
    env.keys.delete(&owner(), first.id).await.unwrap();
    assert!(env.keys.list(&owner()).await.unwrap().is_empty());
    assert!(env.secret_paths().await.is_empty());
    assert!(matches!(
        env.keys.delete(&owner(), first.id).await,
        Err(SshKeysError::NotFound)
    ));
    let second = env
        .keys
        .generate(&owner(), KeyType::Ed25519, None)
        .await
        .unwrap();
    assert!(second.id > first.id);
    assert_eq!(
        env.secret_paths().await,
        [format!("users/owner/ssh-keys/{}", second.id)]
    );
}

#[tokio::test]
async fn a_failed_secret_write_leaves_no_row() {
    let env = Env::new().await;
    env.secrets.fail_put.store(true, Ordering::SeqCst);
    let err = env
        .keys
        .generate(&owner(), KeyType::Ed25519, None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, SshKeysError::Secrets(SecretError::Unavailable(_))),
        "{err:?}"
    );
    let err = env
        .keys
        .upload(&owner(), &fixture("rsa"), None, None)
        .await
        .unwrap_err();
    assert!(matches!(err, SshKeysError::Secrets(_)), "{err:?}");
    assert!(env.keys.list(&owner()).await.unwrap().is_empty());
    assert!(env.store.ssh_keys_all().await.unwrap().is_empty());
    // And nothing is stuck: the same key uploads once the store is back.
    env.secrets.fail_put.store(false, Ordering::SeqCst);
    env.keys
        .upload(&owner(), &fixture("rsa"), None, None)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_failed_secret_delete_still_deletes_the_key() {
    let env = Env::new().await;
    let key = env
        .keys
        .generate(&owner(), KeyType::Ed25519, None)
        .await
        .unwrap();
    env.secrets.fail_delete.store(true, Ordering::SeqCst);
    env.keys.delete(&owner(), key.id).await.unwrap();
    assert!(env.keys.list(&owner()).await.unwrap().is_empty());
    // The sealed value stays behind, unreachable: no row names it.
    assert_eq!(env.secret_paths().await.len(), 1);
    assert!(matches!(
        env.keys.private_key(&owner(), key.id).await,
        Err(SshKeysError::NotFound)
    ));
}

#[tokio::test]
async fn a_user_id_that_cant_name_a_secret_cant_keep_keys() {
    let env = Env::new().await;
    let loud = user("Alice");
    let err = env
        .keys
        .generate(&loud, KeyType::Ed25519, None)
        .await
        .unwrap_err();
    assert!(matches!(err, SshKeysError::UserCantKeepKeys(_)), "{err:?}");
    let err = env
        .keys
        .upload(&loud, &fixture("rsa"), None, None)
        .await
        .unwrap_err();
    assert!(matches!(err, SshKeysError::UserCantKeepKeys(_)), "{err:?}");
    assert!(env.store.ssh_keys_all().await.unwrap().is_empty());
}

#[tokio::test]
async fn users_see_and_change_only_their_own_keys() {
    let env = Env::new().await;
    let alice = user("alice");
    let bob = user("bob");
    let hers = env
        .keys
        .generate(&alice, KeyType::Ed25519, Some("hers"))
        .await
        .unwrap();
    let mine = env
        .keys
        .generate(&bob, KeyType::Ed25519, Some("mine"))
        .await
        .unwrap();

    assert_eq!(
        env.keys.list(&alice).await.unwrap(),
        std::slice::from_ref(&hers)
    );
    assert_eq!(
        env.keys.list(&bob).await.unwrap(),
        std::slice::from_ref(&mine)
    );
    assert!(env.keys.list(&user("carol")).await.unwrap().is_empty());

    // Another user's key is "not found", the same as a key that doesn't exist.
    let missing = SshKeyId::new(9999);
    for (what, result) in [
        ("get", env.keys.get(&bob, hers.id).await.map(|_| ())),
        ("get missing", env.keys.get(&bob, missing).await.map(|_| ())),
        (
            "rename",
            env.keys.rename(&bob, hers.id, "x").await.map(|_| ()),
        ),
        ("delete", env.keys.delete(&bob, hers.id).await),
        (
            "private key",
            env.keys.private_key(&bob, hers.id).await.map(|_| ()),
        ),
    ] {
        assert!(
            matches!(result, Err(SshKeysError::NotFound)),
            "{what}: {result:?}"
        );
    }
    // Not even the owner can use or manage another user's key as their own.
    assert!(matches!(
        env.keys.private_key(&owner(), hers.id).await,
        Err(SshKeysError::NotFound)
    ));
    assert!(matches!(
        env.keys.delete(&owner(), hers.id).await,
        Err(SshKeysError::NotFound)
    ));
    assert_eq!(
        env.keys.get(&alice, hers.id).await.unwrap(),
        hers,
        "untouched"
    );
    assert!(env.stored_pem(&alice, hers.id).await.is_some());
}

#[tokio::test]
async fn only_admins_get_the_admin_calls() {
    let env = Env::new().await;
    let alice = user("alice");
    let second = user("admin2");
    let hers = env
        .keys
        .generate(&alice, KeyType::Ed25519, Some("hers"))
        .await
        .unwrap();
    let mine = env
        .keys
        .generate(&owner(), KeyType::Ed25519, Some("mine"))
        .await
        .unwrap();

    assert!(env.keys.may_administer(&owner()));
    assert!(env.keys.may_administer(&second));
    assert!(!env.keys.may_administer(&alice));
    assert!(!env.keys.may_administer(&user("nobody")));
    assert!(matches!(
        env.keys.all_keys(&alice).await,
        Err(SshKeysError::Forbidden)
    ));
    assert!(matches!(
        env.keys.any_key(&alice, mine.id).await,
        Err(SshKeysError::Forbidden)
    ));
    assert!(matches!(
        env.keys.delete_any(&alice, mine.id).await,
        Err(SshKeysError::Forbidden)
    ));
    assert!(env.stored_pem(&owner(), mine.id).await.is_some());

    // Every admin sees every user's metadata, ordered by user.
    for admin in [owner(), second.clone()] {
        let all = env.keys.all_keys(&admin).await.unwrap();
        assert_eq!(all, [hers.clone(), mine.clone()]);
        assert_eq!(env.keys.any_key(&admin, hers.id).await.unwrap(), hers);
    }
    assert!(matches!(
        env.keys.any_key(&owner(), SshKeyId::new(9999)).await,
        Err(SshKeysError::NotFound)
    ));

    // A second admin can delete a user's key, row and secret, but not use it.
    assert!(matches!(
        env.keys.private_key(&second, hers.id).await,
        Err(SshKeysError::NotFound)
    ));
    let gone = env.keys.delete_any(&second, hers.id).await.unwrap();
    assert_eq!(gone, hers);
    assert!(env.keys.list(&alice).await.unwrap().is_empty());
    assert!(env.stored_pem(&alice, hers.id).await.is_none());
    assert!(matches!(
        env.keys.delete_any(&owner(), hers.id).await,
        Err(SshKeysError::NotFound)
    ));
    assert_eq!(env.keys.list(&owner()).await.unwrap(), [mine]);
}

#[tokio::test]
async fn the_admin_calls_follow_the_users_as_they_change() {
    let env = Env::new().await;
    let second = user("admin2");
    assert!(env.keys.all_keys(&second).await.is_ok());

    // Demoted: the next call is refused. Disabled: so is an admin's.
    let mut demoted = User::member("admin2", &[]);
    demoted.role = Role::Member;
    let mut disabled = User::admin("owner", &[]);
    disabled.state = UserState::Disabled;
    env.users.set(vec![demoted, disabled]);
    for acting in [second, owner()] {
        assert!(!env.keys.may_administer(&acting), "{acting}");
        assert!(matches!(
            env.keys.all_keys(&acting).await,
            Err(SshKeysError::Forbidden)
        ));
    }
}

#[tokio::test]
async fn the_signing_path_gets_keys_most_recently_used_first_and_records_use() {
    let env = Env::new().await;
    let alice = user("alice");
    env.clock.set(0);
    let a = env
        .keys
        .generate(&owner(), KeyType::Ed25519, Some("a"))
        .await
        .unwrap();
    env.clock.set(10);
    let b = env
        .keys
        .generate(&owner(), KeyType::EcdsaP256, Some("b"))
        .await
        .unwrap();
    env.clock.set(20);
    let c = env
        .keys
        .generate(&owner(), KeyType::Ed25519, Some("c"))
        .await
        .unwrap();
    env.keys
        .generate(&alice, KeyType::Ed25519, Some("not mine"))
        .await
        .unwrap();
    let labels = |keys: Vec<SshKey>| keys.into_iter().map(|k| k.label).collect::<Vec<_>>();

    assert_eq!(
        labels(env.keys.agent_keys(&owner()).await.unwrap()),
        ["c", "b", "a"]
    );
    let at = |s: i64| Timestamp::from_second(1_800_000_000 + s).unwrap();
    env.keys.touch_used(a.id, at(30)).await.unwrap();
    assert_eq!(
        labels(env.keys.agent_keys(&owner()).await.unwrap()),
        ["a", "c", "b"]
    );
    env.keys.touch_used(b.id, at(40)).await.unwrap();
    assert_eq!(
        labels(env.keys.agent_keys(&owner()).await.unwrap()),
        ["b", "a", "c"]
    );
    assert_eq!(
        env.keys.get(&owner(), b.id).await.unwrap().last_used_at,
        Some(at(40))
    );
    assert_eq!(
        env.keys.get(&owner(), c.id).await.unwrap().last_used_at,
        None
    );
    // The page's own order doesn't move.
    assert_eq!(
        labels(env.keys.list(&owner()).await.unwrap()),
        ["a", "b", "c"]
    );
    assert!(matches!(
        env.keys.touch_used(SshKeyId::new(9999), at(50)).await,
        Err(SshKeysError::NotFound)
    ));
    assert!(
        env.keys
            .agent_keys(&user("nobody"))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn the_private_key_comes_back_decrypted_and_can_sign() {
    let env = Env::new().await;
    // An encrypted upload is stored decrypted, so nobody has to type a passphrase later.
    let key = env.upload("rsa-encrypted", Some(PASS)).await;
    let private = env.keys.private_key(&owner(), key.id).await.unwrap();
    assert!(!private.is_encrypted());
    assert_eq!(
        private.fingerprint(HashAlg::Sha256).to_string(),
        fixture_fingerprint("rsa-encrypted.pub")
    );
    let signature = ssh_sig(&private, b"hello");
    PublicKey::from_openssh(&key.public_key)
        .unwrap()
        .verify("file", b"hello", &signature)
        .unwrap();
}

#[tokio::test]
async fn a_damaged_secret_is_reported_not_returned() {
    let env = Env::new().await;
    let key = env
        .keys
        .generate(&owner(), KeyType::Ed25519, None)
        .await
        .unwrap();
    let path = SecretPath::new(format!("users/owner/ssh-keys/{}", key.id)).unwrap();
    env.secrets
        .put(&path, &Secret::new("not a key"), None)
        .await
        .unwrap();
    let err = env.keys.private_key(&owner(), key.id).await.unwrap_err();
    assert!(
        matches!(err, SshKeysError::Secrets(SecretError::Damaged { .. })),
        "{err:?}"
    );
    assert!(!err.to_string().contains("not a key"));
}

#[tokio::test]
async fn nothing_prints_or_lists_a_private_key() {
    let env = Env::new().await;
    let key = env
        .keys
        .generate(&owner(), KeyType::Ed25519, None)
        .await
        .unwrap();
    let pem = env.stored_pem(&owner(), key.id).await.unwrap();
    let body = pem.lines().nth(1).unwrap();
    let shown = format!(
        "{key:?} {:?} {:?}",
        env.keys,
        env.keys.list(&owner()).await.unwrap()
    );
    assert!(!shown.contains(body));
    assert!(!shown.contains("PRIVATE"));
    // The secret store's own listing carries paths and metadata, not values.
    let listed = format!("{:?}", env.secrets.list(None).await.unwrap());
    assert!(!listed.contains(body));
}
