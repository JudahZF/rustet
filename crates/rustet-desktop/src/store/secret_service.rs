//! The OSCrypt password in the freedesktop Secret Service, as Chromium's
//! `key_storage_libsecret.cc` keeps it: an item with the attribute `application=<app name>`
//! (schema `chrome_libsecret_os_crypt_password_v2`), labelled `Chromium Safe Storage`.

use std::collections::HashMap;

use futures::StreamExt;
use zbus::Connection;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

use super::keyring::{KeyringError, new_password};

const SCHEMA: &str = "chrome_libsecret_os_crypt_password_v2";
const LABEL: &str = "Chromium Safe Storage";

/// A secret as the Secret Service transfers it: session, parameters, value, content type.
type Secret = (OwnedObjectPath, Vec<u8>, Vec<u8>, String);

#[zbus::proxy(
    interface = "org.freedesktop.Secret.Service",
    default_service = "org.freedesktop.secrets",
    default_path = "/org/freedesktop/secrets"
)]
trait Service {
    fn open_session(
        &self,
        algorithm: &str,
        input: &Value<'_>,
    ) -> zbus::Result<(OwnedValue, OwnedObjectPath)>;
    fn search_items(
        &self,
        attributes: HashMap<&str, &str>,
    ) -> zbus::Result<(Vec<OwnedObjectPath>, Vec<OwnedObjectPath>)>;
    fn unlock(
        &self,
        objects: &[ObjectPath<'_>],
    ) -> zbus::Result<(Vec<OwnedObjectPath>, OwnedObjectPath)>;
    fn get_secrets(
        &self,
        items: &[ObjectPath<'_>],
        session: &ObjectPath<'_>,
    ) -> zbus::Result<HashMap<OwnedObjectPath, Secret>>;
    fn read_alias(&self, name: &str) -> zbus::Result<OwnedObjectPath>;
    fn create_collection(
        &self,
        properties: HashMap<&str, Value<'_>>,
        alias: &str,
    ) -> zbus::Result<(OwnedObjectPath, OwnedObjectPath)>;
}

#[zbus::proxy(
    interface = "org.freedesktop.Secret.Collection",
    default_service = "org.freedesktop.secrets"
)]
trait Collection {
    fn create_item(
        &self,
        properties: HashMap<&str, Value<'_>>,
        secret: &(ObjectPath<'_>, Vec<u8>, Vec<u8>, &str),
        replace: bool,
    ) -> zbus::Result<(OwnedObjectPath, OwnedObjectPath)>;
}

#[zbus::proxy(
    interface = "org.freedesktop.Secret.Prompt",
    default_service = "org.freedesktop.secrets"
)]
trait Prompt {
    fn prompt(&self, window_id: &str) -> zbus::Result<()>;
    #[zbus(signal)]
    fn completed(&self, dismissed: bool, result: Value<'_>) -> zbus::Result<()>;
}

fn is_none(path: &ObjectPath<'_>) -> bool {
    path.as_str() == "/"
}

/// Runs a prompt the service returned and waits for the user. `Ok(Some(result))` when it
/// completed, `Ok(None)` when dismissed.
async fn run_prompt(
    conn: &Connection,
    path: OwnedObjectPath,
) -> Result<Option<OwnedValue>, KeyringError> {
    let prompt = PromptProxy::builder(conn).path(path)?.build().await?;
    let mut completed = prompt.receive_completed().await?;
    prompt.prompt("").await?;
    let Some(signal) = completed.next().await else {
        return Err(KeyringError::Refused("the prompt went away"));
    };
    let args = signal.args()?;
    Ok((!args.dismissed)
        .then(|| args.result.try_to_owned())
        .transpose()
        .map_err(zbus::Error::from)?)
}

/// Unlocks `objects`, prompting when the service asks to.
async fn unlock(
    conn: &Connection,
    service: &ServiceProxy<'_>,
    objects: &[OwnedObjectPath],
) -> Result<(), KeyringError> {
    let paths: Vec<ObjectPath<'_>> = objects.iter().map(|p| p.as_ref()).collect();
    let (_, prompt) = service.unlock(&paths).await?;
    if !is_none(&prompt) && run_prompt(conn, prompt).await?.is_none() {
        return Err(KeyringError::Refused("unlock dismissed"));
    }
    Ok(())
}

/// The password stored under one of `app_names` (tried in order), or a new one stored under the
/// first. Lookup matches only the `application` attribute, as Chromium's does.
pub(crate) async fn password(app_names: &[&str]) -> Result<String, KeyringError> {
    let conn = Connection::session().await?;
    let service = ServiceProxy::new(&conn).await?;
    let (_, session) = service.open_session("plain", &Value::from("")).await?;
    for name in app_names {
        let (unlocked, locked) = service
            .search_items(HashMap::from([("application", *name)]))
            .await?;
        let item = match (unlocked.into_iter().next(), locked.into_iter().next()) {
            (Some(item), _) => item,
            (None, Some(item)) => {
                unlock(&conn, &service, std::slice::from_ref(&item)).await?;
                item
            }
            (None, None) => continue,
        };
        let secrets = service
            .get_secrets(&[item.as_ref()], &session.as_ref())
            .await?;
        let (_, _, value, _) = secrets
            .into_values()
            .next()
            .ok_or(KeyringError::Refused("the item has no secret"))?;
        return String::from_utf8(value)
            .map_err(|_| KeyringError::Refused("the secret is not text"));
    }
    let password = new_password()?;
    store(&conn, &service, &session, app_names[0], &password).await?;
    Ok(password)
}

/// Stores `password` in the default collection (creating it when there is none).
async fn store(
    conn: &Connection,
    service: &ServiceProxy<'_>,
    session: &OwnedObjectPath,
    app: &str,
    password: &str,
) -> Result<(), KeyringError> {
    let mut collection = service.read_alias("default").await?;
    if is_none(&collection) {
        let props = HashMap::from([(
            "org.freedesktop.Secret.Collection.Label",
            Value::from("Default Keyring"),
        )]);
        let (created, prompt) = service.create_collection(props, "default").await?;
        collection = if is_none(&prompt) {
            created
        } else {
            let result = run_prompt(conn, prompt)
                .await?
                .ok_or(KeyringError::Refused("keyring creation dismissed"))?;
            OwnedObjectPath::try_from(result).map_err(zbus::Error::from)?
        };
    }
    unlock(conn, service, std::slice::from_ref(&collection)).await?;
    let attributes = HashMap::from([
        ("xdg:schema".to_owned(), SCHEMA.to_owned()),
        ("application".to_owned(), app.to_owned()),
    ]);
    let props = HashMap::from([
        ("org.freedesktop.Secret.Item.Label", Value::from(LABEL)),
        (
            "org.freedesktop.Secret.Item.Attributes",
            Value::from(attributes),
        ),
    ]);
    let secret = (
        session.as_ref(),
        Vec::new(),
        password.as_bytes().to_vec(),
        "text/plain",
    );
    let target = CollectionProxy::builder(conn)
        .path(collection)?
        .build()
        .await?;
    let (_, prompt) = target.create_item(props, &secret, true).await?;
    if !is_none(&prompt) && run_prompt(conn, prompt).await?.is_none() {
        return Err(KeyringError::Refused("saving the password dismissed"));
    }
    Ok(())
}
