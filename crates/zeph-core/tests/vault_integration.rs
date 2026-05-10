// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Integration tests for vault + config resolution.

use std::io::Write as _;
use std::path::Path;

use age::secrecy::ExposeSecret;

use zeph_core::config::SecretResolver;
use zeph_vault::{AgeVaultError, AgeVaultProvider};

fn encrypt_json(identity: &age::x25519::Identity, json: &serde_json::Value) -> Vec<u8> {
    let recipient = identity.to_public();
    let encryptor =
        age::Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient))
            .expect("encryptor creation");
    let mut encrypted = vec![];
    let mut writer = encryptor.wrap_output(&mut encrypted).expect("wrap_output");
    writer
        .write_all(json.to_string().as_bytes())
        .expect("write plaintext");
    writer.finish().expect("finish encryption");
    encrypted
}

fn write_temp_files(
    identity: &age::x25519::Identity,
    ciphertext: &[u8],
) -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let key_path = dir.path().join("key.txt");
    let vault_path = dir.path().join("secrets.age");
    std::fs::write(&key_path, identity.to_string().expose_secret()).expect("write key");
    std::fs::write(&vault_path, ciphertext).expect("write vault");
    (dir, key_path, vault_path)
}

#[tokio::test]
async fn age_encrypt_decrypt_resolve_secrets_roundtrip() {
    let identity = age::x25519::Identity::generate();
    let json = serde_json::json!({
        "ZEPH_CLAUDE_API_KEY": "sk-ant-test-123",
        "ZEPH_TELEGRAM_TOKEN": "tg-token-456"
    });
    let encrypted = encrypt_json(&identity, &json);
    let (_dir, key_path, vault_path) = write_temp_files(&identity, &encrypted);

    let vault = AgeVaultProvider::new(&key_path, &vault_path).unwrap();
    let mut config =
        zeph_core::config::Config::load(Path::new("/nonexistent/config.toml")).unwrap();
    config.resolve_secrets(&vault).await.unwrap();

    assert_eq!(
        config.secrets.claude_api_key.as_ref().unwrap().expose(),
        "sk-ant-test-123"
    );
    // No [telegram] section in config → vault token must NOT auto-create the config.
    assert!(
        config.telegram.is_none(),
        "vault token must not create TelegramConfig when no [telegram] section exists"
    );
}

#[tokio::test]
async fn age_vault_injects_token_into_existing_telegram_config() {
    let identity = age::x25519::Identity::generate();
    let json = serde_json::json!({ "ZEPH_TELEGRAM_TOKEN": "tg-injected-789" });
    let encrypted = encrypt_json(&identity, &json);
    let (_dir, key_path, vault_path) = write_temp_files(&identity, &encrypted);

    let vault = AgeVaultProvider::new(&key_path, &vault_path).unwrap();
    let mut config =
        zeph_core::config::Config::load(Path::new("/nonexistent/config.toml")).unwrap();
    // Pre-populate the telegram section (simulates an explicit [telegram] block in TOML).
    config.telegram = Some(zeph_core::config::TelegramConfig {
        token: None,
        allowed_users: vec!["test_user".to_owned()],
        skills: zeph_core::config::ChannelSkillsConfig::default(),
        stream_interval_ms: 3000,
        guest_mode: false,
        bot_to_bot: false,
        allowed_bots: vec![],
        max_bot_chain_depth: 3,
    });
    config.resolve_secrets(&vault).await.unwrap();

    let tg = config
        .telegram
        .expect("telegram config must still be present");
    assert_eq!(tg.token.as_deref(), Some("tg-injected-789"));
}

// Suppress unused import warning when age is not in scope (satisfies clippy)
#[allow(dead_code)]
fn _use_age_vault_error(_: AgeVaultError) {}
