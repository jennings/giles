//! `RealDiscord::new` installs the crypto provider `twilight-gateway` needs. A test binary of its
//! own: in the same binary as other tests, one of them could have installed the provider first.

use giles_channel_discord::config::{Config, Settings};
use giles_channel_discord::real::RealDiscord;
use giles_plugin_sdk::tls::rustls::crypto::CryptoProvider;

#[tokio::test]
async fn the_client_installs_the_crypto_provider_it_needs() {
    assert!(
        CryptoProvider::get_default().is_none(),
        "nothing in this binary installs one"
    );
    let config: Config = serde_json::from_value(serde_json::json!({
        "token": "MTIzNDU2.Gabcde.not-a-real-token-1234567890"
    }))
    .unwrap();
    // `twilight-gateway` panics building its TLS connector without a process-default provider.
    let _client = RealDiscord::new(&Settings::new(config).unwrap());
    assert!(CryptoProvider::get_default().is_some());
}
