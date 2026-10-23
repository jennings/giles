use serde_json::{Value, json};

use super::*;

const CLIENT_CERT: &str = include_str!("../../tests/fixtures/client.pem");
const CLIENT_KEY: &str = include_str!("../../tests/fixtures/client.key");
const OTHER_KEY: &str = include_str!("../../tests/fixtures/server.key");

/// A minimal valid config with `extra` merged in (`null` removes a key).
fn config(extra: Value) -> Value {
    let mut config = json!({ "server": "irc.example.net", "nick": "giles", "network": "example" });
    for (key, value) in extra.as_object().unwrap() {
        if value.is_null() {
            config.as_object_mut().unwrap().remove(key);
        } else {
            config[key] = value.clone();
        }
    }
    config
}

fn settings(extra: Value) -> Result<Settings> {
    let config: Config = serde_json::from_value(config(extra)).map_err(Error::other)?;
    Settings::new(config)
}

/// The error message for `extra`, which must be invalid.
fn error(extra: Value) -> String {
    match settings(extra) {
        Err(Error::InvalidRequest(message)) => message,
        Err(other) => panic!("not InvalidRequest: {other}"),
        Ok(settings) => panic!("accepted: {settings:?}"),
    }
}

#[test]
fn defaults() {
    let settings = settings(json!({})).unwrap();
    assert_eq!(settings.host, "irc.example.net");
    assert_eq!(settings.port, TLS_PORT);
    assert!(settings.tls);
    assert_eq!(settings.username, "giles");
    assert_eq!(settings.realname, "Giles");
    assert_eq!(settings.namespace(), "irc:example");
    assert!(!settings.mask_identities);
    assert!(settings.channels.is_empty());
    assert_eq!(settings.max_lines, DEFAULT_MAX_LINES);
    assert_eq!(settings.send_burst, DEFAULT_SEND_BURST);
    assert_eq!(
        settings.send_interval,
        Duration::from_millis(DEFAULT_SEND_INTERVAL_MS)
    );
    assert!(settings.sasl.is_none());
    assert!(
        settings
            .tls_config(RootCertStore::empty())
            .unwrap()
            .is_some()
    );
}

#[test]
fn server_forms() {
    let cases = [
        (
            json!({ "server": "irc.example.net:6697" }),
            "irc.example.net",
            6697,
        ),
        (
            json!({ "server": "irc.example.net", "port": 7000 }),
            "irc.example.net",
            7000,
        ),
        (json!({ "server": "[::1]:6697" }), "::1", 6697),
        (
            json!({ "server": "[2001:db8::1]" }),
            "2001:db8::1",
            TLS_PORT,
        ),
        (json!({ "server": "2001:db8::1" }), "2001:db8::1", TLS_PORT),
        (
            json!({ "server": "127.0.0.1", "tls": false }),
            "127.0.0.1",
            PLAIN_PORT,
        ),
    ];
    for (extra, host, port) in cases {
        let settings = settings(extra.clone()).unwrap();
        assert_eq!(
            (settings.host.as_str(), settings.port),
            (host, port),
            "{extra}"
        );
    }
}

#[test]
fn bad_servers_and_ports_name_the_key() {
    for server in [
        "",
        "irc example",
        "host:port",
        "host:70000",
        "[::1",
        "[::1]x",
        ":6697",
    ] {
        let message = error(json!({ "server": server }));
        assert!(
            message.starts_with("invalid config: server: "),
            "{server}: {message}"
        );
    }
    assert_eq!(
        error(json!({ "server": "host:6697", "port": 6697 })),
        "invalid config: port: also given in `server`"
    );
    assert!(error(json!({ "port": 0 })).starts_with("invalid config: port: "));
}

#[test]
fn plain_tcp_needs_a_local_server_or_insecure() {
    assert!(error(json!({ "tls": false })).starts_with("invalid config: tls: "));
    for local in ["localhost", "127.0.0.1", "127.1.2.3", "::1"] {
        let settings = settings(json!({ "server": local, "tls": false })).unwrap();
        assert!(
            settings
                .tls_config(RootCertStore::empty())
                .unwrap()
                .is_none()
        );
    }
    settings(json!({ "tls": false, "insecure": true })).unwrap();
    // But passwords still only go over TLS or locally.
    assert!(
        error(json!({ "tls": false, "insecure": true, "password": "hunter22" }))
            .starts_with("invalid config: password: ")
    );
    assert!(
        error(json!({
            "tls": false, "insecure": true, "sasl": { "password": "hunter22" },
        }))
        .starts_with("invalid config: sasl: ")
    );
}

#[test]
fn names_are_checked() {
    for nick in ["", "9lives", "-dash", "has space", "bad!nick", "a@b", "ünï"] {
        assert!(
            error(json!({ "nick": nick })).starts_with("invalid config: nick: "),
            "{nick}"
        );
    }
    for nick in ["giles", "Giles_", "[bot]", "g-1", "`x", "{a}|^"] {
        settings(json!({ "nick": nick })).unwrap();
    }
    assert!(error(json!({ "username": "a b" })).starts_with("invalid config: username: "));
    assert!(error(json!({ "username": "a@b" })).starts_with("invalid config: username: "));
    assert!(error(json!({ "realname": "" })).starts_with("invalid config: realname: "));
    assert!(error(json!({ "realname": "a\nb" })).starts_with("invalid config: realname: "));
    for network in ["", "libera:chat", "libera chat"] {
        assert!(
            error(json!({ "network": network })).starts_with("invalid config: network: "),
            "{network}"
        );
    }
}

#[test]
fn channels_are_checked_by_index() {
    let settings = settings(json!({ "channels": ["#giles", "&local", "##meta"] })).unwrap();
    assert_eq!(settings.channels, ["#giles", "&local", "##meta"]);
    for (bad, index) in [
        (json!(["#ok", "nohash"]), 1),
        (json!(["#a b"]), 0),
        (json!(["#ok", "#ok", "#a,#b"]), 2),
        (json!(["#"]), 0),
        (json!(["#a:b"]), 0),
    ] {
        let message = error(json!({ "channels": bad }));
        assert!(
            message.starts_with(&format!("invalid config: channels[{index}]: ")),
            "{message}"
        );
    }
}

#[test]
fn limits_are_checked() {
    assert!(error(json!({ "max_line_bytes": 15 })).starts_with("invalid config: max_line_bytes: "));
    assert_eq!(
        settings(json!({ "max_line_bytes": 16 }))
            .unwrap()
            .max_line_bytes,
        Some(16)
    );
    assert!(error(json!({ "max_lines": 0 })).starts_with("invalid config: max_lines: "));
    assert!(error(json!({ "send_burst": 0 })).starts_with("invalid config: send_burst: "));
    let settings = settings(json!({ "send_interval_ms": 0 })).unwrap();
    assert_eq!(settings.send_interval, Duration::ZERO);
}

#[test]
fn unknown_keys_and_wrong_types_are_refused() {
    let unknown = serde_json::from_value::<Config>(config(json!({ "nickname": "x" })));
    assert!(unknown.err().unwrap().to_string().contains("nickname"));
    let wrong = serde_json::from_value::<Config>(config(json!({ "sasl": { "mechanism": "md5" } })));
    assert!(wrong.err().unwrap().to_string().contains("md5"));
    for missing in ["server", "nick", "network"] {
        let error = serde_json::from_value::<Config>(config(json!({ missing: null })));
        assert!(
            error.err().unwrap().to_string().contains(missing),
            "{missing}"
        );
    }
}

#[test]
fn sasl_plain() {
    let settings = settings(json!({ "sasl": { "password": "hunter22" } })).unwrap();
    assert_eq!(
        settings.sasl,
        Some(Sasl::Plain {
            username: "giles".into(),
            password: Secret("hunter22".into()),
        })
    );
    let named = self::settings(json!({
        "sasl": { "mechanism": "plain", "username": "acct", "password": "hunter22" },
    }))
    .unwrap();
    assert!(matches!(named.sasl, Some(Sasl::Plain { ref username, .. }) if username == "acct"));
    // Secrets never show in Debug output or errors.
    assert!(!format!("{settings:?}").contains("hunter22"));
    let message = error(json!({ "sasl": { "password": "two\nlines" } }));
    assert_eq!(
        message,
        "invalid config: sasl.password: must be non-empty, on one line"
    );
    assert_eq!(
        error(json!({ "sasl": {} })),
        "invalid config: sasl.password: required for mechanism `plain`"
    );
}

#[test]
fn sasl_external_needs_a_client_certificate() {
    let cert = json!({ "client_cert": CLIENT_CERT, "client_key": CLIENT_KEY });
    let mut with_cert = cert.clone();
    with_cert["sasl"] = json!({ "mechanism": "external" });
    let settings = settings(with_cert).unwrap();
    assert_eq!(settings.sasl, Some(Sasl::External));
    assert!(
        settings
            .tls_config(RootCertStore::empty())
            .unwrap()
            .is_some()
    );

    assert!(
        error(json!({ "sasl": { "mechanism": "external" } }))
            .starts_with("invalid config: sasl.mechanism: ")
    );
    let mut with_password = cert;
    with_password["sasl"] = json!({ "mechanism": "external", "password": "x" });
    assert!(error(with_password).starts_with("invalid config: sasl.password: "));
}

#[test]
fn client_certificates_are_checked() {
    assert!(
        error(json!({ "client_cert": CLIENT_CERT })).starts_with("invalid config: client_key: ")
    );
    assert!(
        error(json!({ "client_key": CLIENT_KEY })).starts_with("invalid config: client_cert: ")
    );
    assert!(
        error(json!({ "client_cert": "not pem", "client_key": CLIENT_KEY }))
            .starts_with("invalid config: client_cert: ")
    );
    assert!(
        error(json!({ "client_cert": CLIENT_CERT, "client_key": "not pem" }))
            .starts_with("invalid config: client_key: ")
    );
    // A key that doesn't belong to the certificate.
    let message = error(json!({ "client_cert": CLIENT_CERT, "client_key": OTHER_KEY }));
    assert!(
        message.starts_with("invalid config: client_key: "),
        "{message}"
    );
    assert!(!message.contains("PRIVATE"), "{message}");
    assert!(
        error(json!({
            "server": "127.0.0.1", "tls": false, "client_cert": CLIENT_CERT,
            "client_key": CLIENT_KEY,
        }))
        .starts_with("invalid config: client_cert: ")
    );
}
