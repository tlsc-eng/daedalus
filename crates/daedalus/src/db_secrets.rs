// Used from DB-7.
#![allow(dead_code)]

use anyhow::{Result, anyhow};
use daedalus_db::Secret;
use security_framework::passwords::{
    delete_generic_password, get_generic_password, set_generic_password,
};

const SERVICE: &str = "Daedalus Database";
const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;
const ERR_SEC_USER_CANCELED: i32 = -128;

#[derive(Debug, thiserror::Error)]
pub enum KeychainError {
    #[error("keychain access denied")]
    Denied,
    #[error("keychain error {code}: {message}")]
    Os { code: i32, message: String },
}

fn map(e: security_framework::base::Error) -> KeychainError {
    match e.code() {
        ERR_SEC_USER_CANCELED => KeychainError::Denied,
        code => KeychainError::Os {
            code,
            message: e.message().unwrap_or_default(),
        },
    }
}

/// Blocks on a Keychain prompt, so call it from the background executor.
pub fn save(account: &str, pw: &Secret) -> Result<()> {
    set_generic_password(SERVICE, account, pw.expose().as_bytes()).map_err(|e| map(e).into())
}

fn secret_from_bytes(bytes: Vec<u8>) -> Result<Secret> {
    String::from_utf8(bytes)
        .map(Secret::new)
        .map_err(|_| anyhow!("keychain item is not valid UTF-8"))
}

/// Blocks on a Keychain prompt, so call it from the background executor.
pub fn load(account: &str) -> Result<Option<Secret>> {
    match get_generic_password(SERVICE, account) {
        Ok(bytes) => secret_from_bytes(bytes).map(Some),
        Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
        Err(e) => Err(map(e).into()),
    }
}

/// Blocks on a Keychain prompt, so call it from the background executor.
pub fn delete(account: &str) -> Result<()> {
    match delete_generic_password(SERVICE, account) {
        Err(e) if e.code() != ERR_SEC_ITEM_NOT_FOUND => Err(map(e).into()),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_utf8_error_does_not_hold_the_bytes() {
        let err = secret_from_bytes(b"hunter2\xff".to_vec()).unwrap_err();
        let dump = format!("{err:#?} {err:#}");
        assert!(!dump.contains("hunter2") && !dump.contains("104, 117"));
    }

    struct Cleanup(String);

    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = delete(&self.0);
        }
    }

    #[test]
    #[ignore = "touches the login keychain"]
    fn keychain_round_trip() {
        let account = format!("daedalus-test-{}", std::process::id());
        let _cleanup = Cleanup(account.clone());
        assert!(load(&account).unwrap().is_none());
        save(&account, &Secret::new("pw-one")).unwrap();
        assert_eq!(load(&account).unwrap().unwrap().expose(), "pw-one");
        save(&account, &Secret::new("pw-two")).unwrap();
        assert_eq!(load(&account).unwrap().unwrap().expose(), "pw-two");
        delete(&account).unwrap();
        assert!(load(&account).unwrap().is_none());
        delete(&account).unwrap();
    }
}
