//! The OSCrypt password in KWallet, as Chromium's `key_storage_kwallet.cc` keeps it: folder
//! `Chromium Keys`, entry `Chromium Safe Storage`, in the network wallet.

use zbus::Connection;

use super::keyring::{KWalletVersion, KeyringError, new_password};

const FOLDER: &str = "Chromium Keys";
const KEY: &str = "Chromium Safe Storage";

#[zbus::proxy(interface = "org.kde.KWallet")]
trait KWallet {
    #[zbus(name = "isEnabled")]
    fn is_enabled(&self) -> zbus::Result<bool>;
    #[zbus(name = "networkWallet")]
    fn network_wallet(&self) -> zbus::Result<String>;
    #[zbus(name = "open")]
    fn open(&self, wallet: &str, w_id: i64, appid: &str) -> zbus::Result<i32>;
    #[zbus(name = "hasFolder")]
    fn has_folder(&self, handle: i32, folder: &str, appid: &str) -> zbus::Result<bool>;
    #[zbus(name = "createFolder")]
    fn create_folder(&self, handle: i32, folder: &str, appid: &str) -> zbus::Result<bool>;
    #[zbus(name = "readPassword")]
    fn read_password(
        &self,
        handle: i32,
        folder: &str,
        key: &str,
        appid: &str,
    ) -> zbus::Result<String>;
    #[zbus(name = "writePassword")]
    fn write_password(
        &self,
        handle: i32,
        folder: &str,
        key: &str,
        value: &str,
        appid: &str,
    ) -> zbus::Result<i32>;
    #[zbus(name = "close")]
    fn close(&self, handle: i32, force: bool, appid: &str) -> zbus::Result<i32>;
}

fn endpoint(version: KWalletVersion) -> (&'static str, &'static str) {
    match version {
        KWalletVersion::Four => ("org.kde.kwalletd", "/modules/kwalletd"),
        KWalletVersion::Five => ("org.kde.kwalletd5", "/modules/kwalletd5"),
        KWalletVersion::Six => ("org.kde.kwalletd6", "/modules/kwalletd6"),
    }
}

/// The saved password, or a new one written to the wallet when the entry is missing or empty.
/// `app` is the id KWallet shows in its permission prompt.
pub(crate) async fn password(version: KWalletVersion, app: &str) -> Result<String, KeyringError> {
    let (service, path) = endpoint(version);
    let conn = Connection::session().await?;
    let wallet = KWalletProxy::builder(&conn)
        .destination(service)?
        .path(path)?
        .build()
        .await?;
    if !wallet.is_enabled().await? {
        return Err(KeyringError::Refused("KWallet is disabled"));
    }
    let name = wallet.network_wallet().await?;
    let handle = wallet.open(&name, 0, app).await?;
    if handle < 0 {
        return Err(KeyringError::Refused("KWallet did not open"));
    }
    let result = read_or_create(&wallet, handle, app).await;
    let _ = wallet.close(handle, false, app).await;
    result
}

async fn read_or_create(
    wallet: &KWalletProxy<'_>,
    handle: i32,
    app: &str,
) -> Result<String, KeyringError> {
    if !wallet.has_folder(handle, FOLDER, app).await?
        && !wallet.create_folder(handle, FOLDER, app).await?
    {
        return Err(KeyringError::Refused("KWallet folder could not be created"));
    }
    let saved = wallet.read_password(handle, FOLDER, KEY, app).await?;
    if !saved.is_empty() {
        return Ok(saved);
    }
    let fresh = new_password()?;
    if wallet
        .write_password(handle, FOLDER, KEY, &fresh, app)
        .await?
        != 0
    {
        return Err(KeyringError::Refused("KWallet did not save the password"));
    }
    Ok(fresh)
}
