//! Router-level tests of the SSH keys pages (`/ssh-keys`, `/admin/ssh-keys`) over a real secret
//! store. The upload fixtures are the ones of `ssh/testdata`.

use std::path::Path;

use doozers_store::SshKeyId;

use super::agents::guest;
use super::*;
use crate::secrets::{KeyFile, LocalSecrets, SecretPath, SecretProvider};
use crate::ssh::{KeyType, SshKeys};

/// The passphrase of the encrypted fixtures.
const PASS: &str = "correct horse battery";

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/ssh/testdata")
            .join(name),
    )
    .unwrap()
}

pub(super) struct Ssh {
    pub(super) env: Env,
    pub(super) keys: SshKeys,
    secrets: Arc<LocalSecrets>,
}

/// What a request got back.
struct Answer {
    status: StatusCode,
    body: String,
    location: Option<String>,
}

impl Ssh {
    pub(super) async fn new() -> Self {
        let env = env().await;
        let root =
            crate::paths::DataRoot::new(env._dir.path(), &env._dir.path().join("tmp")).unwrap();
        let clock: Arc<dyn crate::sessions::Clock> = Arc::new(SystemClock);
        let secrets = Arc::new(
            LocalSecrets::open(
                env.store.clone(),
                &KeyFile::new(&root, &root.secret_key_file()),
                clock.clone(),
            )
            .await
            .unwrap(),
        );
        let keys = SshKeys::new(
            env.store.clone(),
            secrets.clone(),
            clock,
            env.channel.users().clone(),
        );
        Self { env, keys, secrets }
    }

    pub(super) fn router(&self, user: Option<WebUser>) -> axum::Router {
        let app = self
            .env
            .app
            .install_ssh_keys(
                self.env.app.install(RouterBuilder::new()),
                self.keys.clone(),
            )
            .build();
        match user {
            Some(user) => app.layer(Extension(user)),
            None => app,
        }
    }

    async fn answer(response: Response) -> Answer {
        assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
        let status = response.status();
        let location = response
            .headers()
            .get(LOCATION)
            .map(|l| l.to_str().unwrap().to_owned());
        Answer {
            status,
            body: body_text(response).await,
            location,
        }
    }

    /// `method path` as `user`, with `form` (URL-encoded) as the body if given. Every answer must
    /// be `no-store`.
    async fn send(&self, user: &WebUser, method: Method, path: &str, form: Option<&str>) -> Answer {
        Self::answer(call(self.router(Some(user.clone())), method, path, &[], form).await).await
    }

    async fn get(&self, user: &WebUser, path: &str) -> Answer {
        self.send(user, Method::GET, path, None).await
    }

    /// A POST of `pairs` and the user's CSRF token, URL-encoded.
    async fn post(&self, user: &WebUser, path: &str, pairs: &[(&str, &str)]) -> Answer {
        let mut form = format!("csrf={}", user.csrf);
        for (name, value) in pairs {
            form.push_str(&format!("&{name}={}", url_encode(value)));
        }
        self.send(user, Method::POST, path, Some(&form)).await
    }

    /// A `multipart/form-data` POST to the upload form. `fields` are (name, filename, content).
    async fn upload(
        &self,
        user: &WebUser,
        csrf: Option<&str>,
        fields: &[(&str, Option<&str>, &[u8])],
    ) -> Answer {
        let boundary = "----doozers-test-boundary";
        let mut body = Vec::new();
        let mut add = |name: &str, filename: Option<&str>, content: &[u8]| {
            body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            match filename {
                Some(file) => body.extend_from_slice(
                    format!(
                        "Content-Disposition: form-data; name=\"{name}\"; filename=\"{file}\"\r\n\
                         Content-Type: application/octet-stream\r\n\r\n"
                    )
                    .as_bytes(),
                ),
                None => body.extend_from_slice(
                    format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
                ),
            }
            body.extend_from_slice(content);
            body.extend_from_slice(b"\r\n");
        };
        if let Some(csrf) = csrf {
            add("csrf", None, csrf.as_bytes());
        }
        for (name, filename, content) in fields {
            add(name, *filename, content);
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        let request = Request::builder()
            .method(Method::POST)
            .uri("/ssh-keys/upload")
            .header(
                CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(body))
            .unwrap();
        let response = self
            .router(Some(user.clone()))
            .oneshot(request)
            .await
            .unwrap();
        Self::answer(response).await
    }

    /// Uploads `file` of the fixtures as pasted text as `user`.
    async fn paste(&self, user: &WebUser, file: &str, passphrase: &str, label: &str) -> Answer {
        let text = fixture(file);
        self.upload(
            user,
            Some(&user.csrf),
            &[
                ("key_text", None, &text),
                ("passphrase", None, passphrase.as_bytes()),
                ("label", None, label.as_bytes()),
            ],
        )
        .await
    }

    async fn keys_of(&self, user: &WebUser) -> Vec<crate::ssh::SshKey> {
        self.keys.list(&user.user).await.unwrap()
    }

    async fn stored_secret(&self, user: &WebUser, id: SshKeyId) -> Option<String> {
        let path = SecretPath::new(format!("users/{}/ssh-keys/{id}", user.user)).unwrap();
        self.secrets
            .get(&path)
            .await
            .unwrap()
            .map(|s| s.expose().to_owned())
    }

    /// `user`'s key with `label`.
    async fn key(&self, user: &WebUser, label: &str) -> crate::ssh::SshKey {
        self.keys_of(user)
            .await
            .into_iter()
            .find(|k| k.label == label)
            .unwrap_or_else(|| panic!("no key labelled {label}"))
    }
}

pub(super) fn url_encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' => (b as char).to_string(),
            b' ' => "+".to_owned(),
            b => format!("%{b:02X}"),
        })
        .collect()
}

#[tokio::test]
async fn every_page_needs_a_login_and_the_admin_pages_an_admin() {
    let ssh = Ssh::new().await;
    let key = ssh
        .keys
        .generate(&owner().user, KeyType::Ed25519, None)
        .await
        .unwrap();
    let id = key.id;
    let user_pages = [
        (Method::GET, "/ssh-keys".to_owned()),
        (Method::POST, "/ssh-keys/generate".to_owned()),
        (Method::POST, "/ssh-keys/upload".to_owned()),
        (Method::GET, format!("/ssh-keys/{id}/rename")),
        (Method::POST, format!("/ssh-keys/{id}/rename")),
        (Method::GET, format!("/ssh-keys/{id}/delete")),
        (Method::POST, format!("/ssh-keys/{id}/delete")),
    ];
    let admin_pages = [
        (Method::GET, "/admin/ssh-keys".to_owned()),
        (Method::GET, format!("/admin/ssh-keys/{id}/delete")),
        (Method::POST, format!("/admin/ssh-keys/{id}/delete")),
    ];
    for (method, path) in user_pages.iter().chain(&admin_pages) {
        let form = (*method == Method::POST).then(|| format!("csrf={CSRF}"));
        let response = call(ssh.router(None), method.clone(), path, &[], form.as_deref()).await;
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {path}"
        );
    }
    for (method, path) in &admin_pages {
        let form = (*method == Method::POST).then(|| format!("csrf={CSRF}"));
        let answer = ssh
            .send(&guest(), method.clone(), path, form.as_deref())
            .await;
        assert_eq!(answer.status, StatusCode::FORBIDDEN, "{method} {path}");
    }
    assert_eq!(ssh.keys_of(&owner()).await.len(), 1, "nothing was deleted");
}

#[tokio::test]
async fn every_change_needs_the_csrf_token() {
    let ssh = Ssh::new().await;
    let key = ssh
        .keys
        .generate(&owner().user, KeyType::Ed25519, Some("mine"))
        .await
        .unwrap();
    let id = key.id;
    for path in [
        "/ssh-keys/generate".to_owned(),
        format!("/ssh-keys/{id}/rename"),
        format!("/ssh-keys/{id}/delete"),
        format!("/admin/ssh-keys/{id}/delete"),
    ] {
        for (headers, form) in [
            (vec![], "label=x".to_owned()),
            (vec![], "csrf=wrong&label=x".to_owned()),
            (
                vec![("x-csrf-token", "wrong")],
                format!("csrf={CSRF}&label=x&type=ed25519"),
            ),
        ] {
            let response = call(
                ssh.router(Some(owner())),
                Method::POST,
                &path,
                &headers,
                Some(&form),
            )
            .await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path} {form}");
        }
    }
    // The upload carries the token as a form field.
    for csrf in [None, Some("wrong")] {
        let text = fixture("rsa");
        let answer = ssh
            .upload(&owner(), csrf, &[("key_text", None, &text)])
            .await;
        assert_eq!(answer.status, StatusCode::FORBIDDEN, "{csrf:?}");
    }
    let keys = ssh.keys_of(&owner()).await;
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].label, "mine");
}

#[tokio::test]
async fn the_page_lists_a_key_with_its_details_and_public_key_and_never_the_private_key() {
    let ssh = Ssh::new().await;
    let empty = ssh.get(&owner(), "/ssh-keys").await;
    assert_eq!(empty.status, StatusCode::OK);
    assert!(
        empty.body.contains("You have no SSH keys"),
        "{}",
        empty.body
    );
    // The forms are there for every type.
    for kind in KeyType::ALL {
        assert!(
            empty.body.contains(&format!("value=\"{}\"", kind.id())),
            "{kind:?}"
        );
    }
    assert!(empty.body.contains("action=\"/ssh-keys/generate\""));
    assert!(empty.body.contains("enctype=\"multipart/form-data\""));

    let key = ssh
        .keys
        .generate(
            &owner().user,
            KeyType::EcdsaP384,
            Some("work <b>laptop</b>"),
        )
        .await
        .unwrap();
    let page = ssh.get(&owner(), "/ssh-keys").await.body;
    assert!(
        page.contains("work &#60;b&#62;laptop&#60;/b&#62;"),
        "the label is escaped: {page}"
    );
    assert!(!page.contains("<b>laptop</b>"));
    assert!(page.contains("ECDSA P-384"));
    assert!(page.contains(&key.fingerprint));
    assert!(page.contains("never"), "not yet used");
    // The public key in authorized_keys form, in a read-only field with a copy button.
    assert!(page.contains(&format!("id=\"public-key-{}\"", key.id)));
    assert!(page.contains("readonly"));
    assert!(page.contains(&key.public_key), "{page}");
    assert!(page.contains(&format!("data-copy-from=\"public-key-{}\"", key.id)));
    assert!(page.contains(crate::http::assets::copy_script_path()));
    assert!(!page.contains("<script>"), "no inline script");
    // The links.
    assert!(page.contains(&format!("/ssh-keys/{}/rename", key.id)));
    assert!(page.contains(&format!("/ssh-keys/{}/delete", key.id)));
    // The sidebar.
    assert!(
        page.contains("href=\"/ssh-keys\" aria-current=\"page\">SSH keys</a>"),
        "{page}"
    );

    // Never the private key, on any page.
    let secret = ssh.stored_secret(&owner(), key.id).await.unwrap();
    let body_line = secret.lines().nth(1).unwrap();
    for path in [
        "/ssh-keys".to_owned(),
        format!("/ssh-keys/{}/rename", key.id),
        format!("/ssh-keys/{}/delete", key.id),
        "/admin/ssh-keys".to_owned(),
        format!("/admin/ssh-keys/{}/delete", key.id),
    ] {
        let answer = ssh.get(&owner(), &path).await;
        assert_eq!(answer.status, StatusCode::OK, "{path}");
        assert!(!answer.body.contains(body_line), "{path}");
        assert!(
            !answer.body.contains("-----END OPENSSH PRIVATE KEY-----"),
            "{path}: no key text, only the upload form's placeholder"
        );
    }
}

#[tokio::test]
async fn the_sidebar_links_the_page_for_everyone_and_the_admin_page_for_admins() {
    let ssh = Ssh::new().await;
    let guest_page = ssh.get(&guest(), "/ssh-keys").await.body;
    assert!(guest_page.contains("href=\"/ssh-keys\""));
    assert!(!guest_page.contains("/admin/ssh-keys"));
    let owner_page = ssh.get(&owner(), "/ssh-keys").await.body;
    assert!(owner_page.contains("href=\"/admin/ssh-keys\""));
    // Next to Connections, in the same group.
    let connections = owner_page.find("href=\"/connections\"").unwrap();
    let keys = owner_page.find("href=\"/ssh-keys\"").unwrap();
    let admin = owner_page.find("href=\"/admin\"").unwrap();
    assert!(connections < keys && keys < admin);
}

#[tokio::test]
async fn generating_a_key_of_each_type_adds_it_and_shows_the_notice() {
    let ssh = Ssh::new().await;
    for kind in KeyType::ALL {
        let answer = ssh
            .post(
                &owner(),
                "/ssh-keys/generate",
                &[("type", kind.id()), ("label", kind.id())],
            )
            .await;
        assert_eq!(
            answer.status,
            StatusCode::SEE_OTHER,
            "{kind:?}: {}",
            answer.body
        );
        assert_eq!(
            answer.location.as_deref(),
            Some("/ssh-keys?notice=generated")
        );
        let key = ssh.key(&owner(), kind.id()).await;
        assert_eq!(key.type_label, kind.label());
        assert!(ssh.stored_secret(&owner(), key.id).await.is_some());
    }
    let page = ssh.get(&owner(), "/ssh-keys?notice=generated").await.body;
    assert!(page.contains("Generated the key."));
    // Anything else in `notice` shows nothing.
    let page = ssh.get(&owner(), "/ssh-keys?notice=%3Cb%3E").await.body;
    assert!(!page.contains("banner-info"));
    // The default label.
    ssh.post(
        &owner(),
        "/ssh-keys/generate",
        &[("type", "ed25519"), ("label", "")],
    )
    .await;
    ssh.key(&owner(), "owner@doozers").await;
}

#[tokio::test]
async fn a_generate_form_with_a_bad_type_or_label_shows_the_reason_and_adds_nothing() {
    let ssh = Ssh::new().await;
    let answer = ssh
        .post(
            &owner(),
            "/ssh-keys/generate",
            &[("type", "dsa"), ("label", "x")],
        )
        .await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST);
    assert!(answer.body.contains("Choose a key type."));
    let long = "x".repeat(101);
    let answer = ssh
        .post(
            &owner(),
            "/ssh-keys/generate",
            &[("type", "rsa-3072"), ("label", &long)],
        )
        .await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST);
    assert!(
        answer
            .body
            .contains("the label is 101 characters; the limit is 100")
    );
    assert!(answer.body.contains(&long), "what was typed comes back");
    assert!(
        answer.body.contains("value=\"rsa-3072\" selected"),
        "the type stays chosen: {}",
        answer.body
    );
    assert!(ssh.keys_of(&owner()).await.is_empty());
}

#[tokio::test]
async fn uploading_pasted_text_a_file_or_an_encrypted_key_adds_it() {
    let ssh = Ssh::new().await;
    // Pasted: a browser sends a textarea's line breaks as CRLF.
    let text = String::from_utf8(fixture("ecdsa-p256"))
        .unwrap()
        .replace('\n', "\r\n");
    let answer = ssh
        .upload(
            &owner(),
            Some(CSRF),
            &[
                ("key_text", None, text.as_bytes()),
                ("label", None, b"pasted"),
            ],
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER, "{}", answer.body);
    assert_eq!(
        answer.location.as_deref(),
        Some("/ssh-keys?notice=uploaded")
    );
    let pasted = ssh.key(&owner(), "pasted").await;
    assert_eq!(pasted.algorithm, "ecdsa-sha2-nistp256");

    // A file, which wins over the text; the label comes from the key's comment.
    let rsa = fixture("rsa");
    let answer = ssh
        .upload(
            &owner(),
            Some(CSRF),
            &[
                ("key_text", None, b"ignored"),
                ("key_file", Some("id_rsa"), &rsa),
            ],
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER, "{}", answer.body);
    ssh.key(&owner(), "rsa@fixture").await;

    // An encrypted key in a legacy format, with its passphrase.
    let answer = ssh
        .paste(&owner(), "rsa-pkcs1-encrypted", PASS, "legacy")
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER, "{}", answer.body);
    let legacy = ssh.key(&owner(), "legacy").await;
    assert_eq!(legacy.algorithm, "ssh-rsa");
    // The stored secret has no passphrase.
    let stored = ssh.stored_secret(&owner(), legacy.id).await.unwrap();
    assert!(
        !ssh_key::PrivateKey::from_openssh(&stored)
            .unwrap()
            .is_encrypted()
    );
    assert_eq!(ssh.keys_of(&owner()).await.len(), 3);
}

#[tokio::test]
async fn a_wrong_passphrase_is_refused_and_neither_it_nor_the_key_comes_back() {
    let ssh = Ssh::new().await;
    for file in [
        "ed25519-encrypted",
        "rsa-pkcs1-encrypted",
        "ec-p256-pkcs8-encrypted.pem",
    ] {
        let answer = ssh.paste(&owner(), file, "my wrong passphrase", "x").await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{file}");
        assert!(answer.body.contains("the passphrase is wrong"), "{file}");
        assert!(!answer.body.contains("my wrong passphrase"), "{file}");
        let text = String::from_utf8(fixture(file)).unwrap();
        let body_line = text.lines().nth(4).unwrap();
        assert!(
            !answer.body.contains(body_line),
            "{file}: the key isn't echoed"
        );
        // Without one: asked for it.
        let answer = ssh.paste(&owner(), file, "", "x").await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{file}");
        assert!(answer.body.contains("enter its passphrase"), "{file}");
    }
    assert!(ssh.keys_of(&owner()).await.is_empty());
}

#[tokio::test]
async fn unsupported_and_duplicate_uploads_are_refused_with_their_reasons() {
    let ssh = Ssh::new().await;
    for (file, reason) in [
        ("putty.ppk", "puttygen key.ppk -O private-openssh -o key"),
        ("dsa-traditional", "OpenSSH 10.0 removed DSA"),
        ("ed25519.pub", "that is a public key"),
    ] {
        let answer = ssh.paste(&owner(), file, "", "").await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{file}");
        assert!(answer.body.contains(reason), "{file}: {}", answer.body);
    }
    let answer = ssh
        .upload(&owner(), Some(CSRF), &[("label", None, b"nothing")])
        .await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST);
    assert!(
        answer
            .body
            .contains("paste a private key or choose a key file")
    );
    assert!(
        answer.body.contains("value=\"nothing\""),
        "the label comes back"
    );

    // The same key twice, even in another format, names the existing label.
    assert_eq!(
        ssh.paste(&owner(), "rsa", "", "first").await.status,
        StatusCode::SEE_OTHER
    );
    let answer = ssh.paste(&owner(), "rsa-pkcs8.pem", "", "second").await;
    assert_eq!(answer.status, StatusCode::CONFLICT);
    assert!(
        answer
            .body
            .contains("you already have this key, labelled &#34;first&#34;"),
        "{}",
        answer.body
    );
    // Another user can have the same key.
    assert_eq!(
        ssh.paste(&guest(), "rsa", "", "hers").await.status,
        StatusCode::SEE_OTHER
    );
    assert_eq!(ssh.keys_of(&owner()).await.len(), 1);
    assert_eq!(ssh.keys_of(&guest()).await.len(), 1);
}

#[tokio::test]
async fn an_oversized_upload_is_refused_before_it_is_read() {
    let ssh = Ssh::new().await;
    let big = vec![b'a'; 300 * 1024];
    let answer = ssh
        .upload(&owner(), Some(CSRF), &[("key_file", Some("big"), &big)])
        .await;
    assert_eq!(answer.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(answer.body.contains("too large"), "{}", answer.body);
    // Under the form's limit but over the key's: the service refuses it.
    let big = vec![b'a'; 70 * 1024];
    let answer = ssh
        .upload(&owner(), Some(CSRF), &[("key_file", Some("big"), &big)])
        .await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST);
    assert!(
        answer.body.contains("larger than 64 KiB"),
        "{}",
        answer.body
    );
    assert!(ssh.keys_of(&owner()).await.is_empty());
}

#[tokio::test]
async fn renaming_changes_the_label_and_a_bad_label_shows_the_form_again() {
    let ssh = Ssh::new().await;
    let key = ssh
        .keys
        .generate(&owner().user, KeyType::Ed25519, Some("old"))
        .await
        .unwrap();
    let id = key.id;
    let form = ssh.get(&owner(), &format!("/ssh-keys/{id}/rename")).await;
    assert_eq!(form.status, StatusCode::OK);
    assert!(form.body.contains("value=\"old\""));
    assert!(
        form.body
            .contains(&format!("action=\"/ssh-keys/{id}/rename\""))
    );

    let answer = ssh
        .post(
            &owner(),
            &format!("/ssh-keys/{id}/rename"),
            &[("label", "  ")],
        )
        .await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST);
    assert!(answer.body.contains("be empty"), "{}", answer.body);
    assert_eq!(ssh.key(&owner(), "old").await.id, id);

    let answer = ssh
        .post(
            &owner(),
            &format!("/ssh-keys/{id}/rename"),
            &[("label", "new name")],
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert_eq!(answer.location.as_deref(), Some("/ssh-keys?notice=renamed"));
    let renamed = ssh.key(&owner(), "new name").await;
    assert_eq!(renamed.public_key, key.public_key);
    assert_eq!(renamed.fingerprint, key.fingerprint);
}

#[tokio::test]
async fn deleting_asks_first_then_removes_the_key_and_its_secret() {
    let ssh = Ssh::new().await;
    let key = ssh
        .keys
        .generate(&owner().user, KeyType::Rsa3072, Some("doomed"))
        .await
        .unwrap();
    let keep = ssh
        .keys
        .generate(&owner().user, KeyType::Ed25519, Some("kept"))
        .await
        .unwrap();
    let id = key.id;
    let confirm = ssh.get(&owner(), &format!("/ssh-keys/{id}/delete")).await;
    assert_eq!(confirm.status, StatusCode::OK);
    assert!(confirm.body.contains("Delete doomed?"));
    assert!(confirm.body.contains(&key.fingerprint));
    assert!(
        confirm
            .body
            .contains(&format!("action=\"/ssh-keys/{id}/delete\""))
    );
    assert_eq!(
        ssh.keys_of(&owner()).await.len(),
        2,
        "the confirmation changes nothing"
    );

    let answer = ssh
        .post(&owner(), &format!("/ssh-keys/{id}/delete"), &[])
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert_eq!(answer.location.as_deref(), Some("/ssh-keys?notice=deleted"));
    assert_eq!(ssh.keys_of(&owner()).await, std::slice::from_ref(&keep));
    assert!(ssh.stored_secret(&owner(), id).await.is_none());
    assert!(ssh.stored_secret(&owner(), keep.id).await.is_some());
    let again = ssh
        .post(&owner(), &format!("/ssh-keys/{id}/delete"), &[])
        .await;
    assert_eq!(again.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn another_users_key_is_not_found_on_every_route() {
    let ssh = Ssh::new().await;
    let hers = ssh
        .keys
        .generate(&guest().user, KeyType::Ed25519, Some("hers"))
        .await
        .unwrap();
    let id = hers.id;
    ssh.keys
        .generate(&owner().user, KeyType::Ed25519, Some("mine"))
        .await
        .unwrap();

    // Hers isn't on the owner's page, and the owner's isn't on hers.
    let owner_page = ssh.get(&owner(), "/ssh-keys").await.body;
    assert!(owner_page.contains("mine") && !owner_page.contains("hers"));
    assert!(!owner_page.contains(&hers.fingerprint));
    let guest_page = ssh.get(&guest(), "/ssh-keys").await.body;
    assert!(guest_page.contains("hers") && !guest_page.contains("mine"));

    let missing = format!("{}", id.get() + 1000);
    let reasons = [
        (id.to_string(), "another user's"),
        (missing, "missing"),
        ("abc".to_owned(), "not a number"),
    ];
    for (key, what) in reasons {
        let none = ssh.get(&owner(), &format!("/ssh-keys/{key}/rename")).await;
        assert_eq!(none.status, StatusCode::NOT_FOUND, "{what}");
        for (method, path) in [
            (Method::GET, format!("/ssh-keys/{key}/rename")),
            (Method::GET, format!("/ssh-keys/{key}/delete")),
            (Method::POST, format!("/ssh-keys/{key}/rename")),
            (Method::POST, format!("/ssh-keys/{key}/delete")),
        ] {
            let form = (method == Method::POST).then(|| format!("csrf={CSRF}&label=taken"));
            let answer = ssh
                .send(&owner(), method.clone(), &path, form.as_deref())
                .await;
            assert_eq!(
                answer.status,
                StatusCode::NOT_FOUND,
                "{what}: {method} {path}"
            );
            assert!(
                !answer.body.contains("hers"),
                "{what}: {path} doesn't reveal the key"
            );
        }
    }
    // Nothing of hers changed.
    assert_eq!(ssh.keys_of(&guest()).await, [hers]);
    assert!(ssh.stored_secret(&guest(), id).await.is_some());
}

#[tokio::test]
async fn an_admin_sees_every_users_keys_on_the_admin_page_and_deletes_one() {
    let ssh = Ssh::new().await;
    let hers = ssh
        .keys
        .generate(&guest().user, KeyType::EcdsaP256, Some("her laptop"))
        .await
        .unwrap();
    let mine = ssh
        .keys
        .generate(&owner().user, KeyType::Ed25519, Some("my laptop"))
        .await
        .unwrap();

    let page = ssh.get(&owner(), "/admin/ssh-keys").await;
    assert_eq!(page.status, StatusCode::OK);
    for key in [&hers, &mine] {
        assert!(page.body.contains(&key.label));
        assert!(page.body.contains(&key.fingerprint));
        assert!(page.body.contains(&key.type_label));
        assert!(
            page.body
                .contains(&format!("/admin/ssh-keys/{}/delete", key.id))
        );
    }
    assert!(page.body.contains("<code>guest</code>"));
    // Details only: no public key line either, and nothing to copy.
    assert!(!page.body.contains(&hers.public_key));
    let secret = ssh.stored_secret(&guest(), hers.id).await.unwrap();
    assert!(!page.body.contains(secret.lines().nth(1).unwrap()));

    let confirm = ssh
        .get(&owner(), &format!("/admin/ssh-keys/{}/delete", hers.id))
        .await;
    assert_eq!(confirm.status, StatusCode::OK);
    assert!(confirm.body.contains("Delete her laptop?"));
    assert!(confirm.body.contains("<code>guest</code>"));
    assert!(
        confirm
            .body
            .contains(&format!("action=\"/admin/ssh-keys/{}/delete\"", hers.id))
    );
    assert_eq!(ssh.keys_of(&guest()).await.len(), 1);

    let answer = ssh
        .post(
            &owner(),
            &format!("/admin/ssh-keys/{}/delete", hers.id),
            &[],
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert_eq!(
        answer.location.as_deref(),
        Some("/admin/ssh-keys?notice=deleted")
    );
    assert!(ssh.keys_of(&guest()).await.is_empty());
    assert!(ssh.stored_secret(&guest(), hers.id).await.is_none());
    assert_eq!(ssh.keys_of(&owner()).await, std::slice::from_ref(&mine));
    let page = ssh
        .get(&owner(), "/admin/ssh-keys?notice=deleted")
        .await
        .body;
    assert!(page.contains("Deleted the key."));
    assert!(!page.contains("her laptop"));

    for path in ["/admin/ssh-keys/9999/delete", "/admin/ssh-keys/abc/delete"] {
        assert_eq!(
            ssh.get(&owner(), path).await.status,
            StatusCode::NOT_FOUND,
            "{path}"
        );
        assert_eq!(
            ssh.post(&owner(), path, &[]).await.status,
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
}

#[tokio::test]
async fn the_admin_page_says_so_when_nobody_has_a_key() {
    let ssh = Ssh::new().await;
    let page = ssh.get(&owner(), "/admin/ssh-keys").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("No user has an SSH key."));
}
