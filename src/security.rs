use aes_gcm::{
    Aes256Gcm, KeyInit,
    aead::{Aead, Payload},
};
use anyhow::{Context, Result, anyhow, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use rand::{RngCore, rngs::OsRng};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone)]
pub struct Vault(Aes256Gcm);

impl Vault {
    pub fn new(key: &str) -> Result<Self> {
        let key = hex::decode(key.trim()).context("encryption key must be hexadecimal")?;
        Ok(Self(
            Aes256Gcm::new_from_slice(&key)
                .map_err(|_| anyhow!("encryption key must be 32 bytes"))?,
        ))
    }

    pub fn seal(&self, binding: &str, data: &[u8]) -> Result<String> {
        let mut nonce = [0; 12];
        OsRng.fill_bytes(&mut nonce);
        let mut enc = nonce.to_vec();
        enc.extend(
            self.0
                .encrypt(
                    (&nonce).into(),
                    Payload {
                        msg: data,
                        aad: binding.as_bytes(),
                    },
                )
                .map_err(|_| anyhow!("unable to encrypt data"))?,
        );
        Ok(URL_SAFE_NO_PAD.encode(enc))
    }

    pub fn open(&self, binding: &str, encoded: &str) -> Result<Vec<u8>> {
        let enc = URL_SAFE_NO_PAD.decode(encoded)?;
        let (nonce, ciphertext) = enc
            .split_at_checked(12)
            .context("invalid encrypted value")?;
        self.0
            .decrypt(
                nonce.into(),
                Payload {
                    msg: ciphertext,
                    aad: binding.as_bytes(),
                },
            )
            .map_err(|_| anyhow!("unable to decrypt data"))
    }
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_secs() as i64
}

pub fn token() -> String {
    let mut value = [0; 32];
    OsRng.fill_bytes(&mut value);
    URL_SAFE_NO_PAD.encode(value)
}

pub fn hash(value: &str) -> String {
    hex::encode(Sha256::digest(value))
}

pub fn pkce(value: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(value))
}

pub fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn hmac(key: &[u8]) -> Hmac<Sha256> {
    <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC accepts any key")
}

pub fn telegram_user(init: &str, bot_token: &str) -> Result<String> {
    ensure!(init.len() <= 8192, "oversized Telegram session");
    let mut fields = BTreeMap::new();
    for (key, value) in url::form_urlencoded::parse(init.as_bytes()) {
        ensure!(
            fields.insert(key, value).is_none(),
            "duplicate Telegram field"
        );
    }
    let signature = hex::decode(
        fields
            .remove("hash")
            .context("missing Telegram signature")?
            .as_bytes(),
    )?;
    let message = fields
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut key = hmac(b"WebAppData");
    key.update(bot_token.as_bytes());
    let mut signer = hmac(&key.finalize().into_bytes());
    signer.update(message.as_bytes());
    signer
        .verify_slice(&signature)
        .map_err(|_| anyhow!("invalid Telegram signature"))?;
    let date = fields
        .get("auth_date")
        .context("missing Telegram date")?
        .parse::<i64>()?;
    ensure!(
        (now() - 900..=now() + 30).contains(&date),
        "expired Telegram session"
    );
    let user: serde_json::Value =
        serde_json::from_str(fields.get("user").context("missing Telegram user")?)?;
    Ok(user["id"]
        .as_u64()
        .filter(|id| *id > 0)
        .context("invalid Telegram user")?
        .to_string())
}

pub fn clip(value: &str, max_bytes: usize) -> &str {
    &value[..value.floor_char_boundary(max_bytes.min(value.len()))]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ciphertext_is_bound_to_owner_and_purpose() {
        let vault = Vault::new(&"a0".repeat(32)).unwrap();
        let value = vault.seal("connector:1:abc", b"secret").unwrap();
        assert_eq!(vault.open("connector:1:abc", &value).unwrap(), b"secret");
        assert!(vault.open("connector:2:abc", &value).is_err());
    }

    #[test]
    fn verifies_telegram_signature_and_expiry() {
        let user = r#"{"id":123}"#;
        let date = now();
        let mut secret = hmac(b"WebAppData");
        secret.update(b"fake-token");
        let mut signer = hmac(&secret.finalize().into_bytes());
        signer.update(format!("auth_date={date}\nuser={user}").as_bytes());
        let signature = hex::encode(signer.finalize().into_bytes());
        let encoded = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("user", user)
            .append_pair("auth_date", &date.to_string())
            .append_pair("hash", &signature)
            .finish();
        assert_eq!(telegram_user(&encoded, "fake-token").unwrap(), "123");
        assert!(telegram_user(&encoded, "different-token").is_err());
        assert!(telegram_user(&format!("{encoded}&user=x"), "fake-token").is_err());
    }
}
