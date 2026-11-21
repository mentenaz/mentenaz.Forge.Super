use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};

use anyhow::Result;
use credentials_provider::CredentialsProvider;
use futures::FutureExt as _;
use gpui::{App, AsyncApp, Global};
use release_channel::ReleaseChannel;

/// An environment variable whose presence makes a development build store
/// credentials in a plain-text file instead of the system keychain.
///
/// Upstream Zed defaults to the file in development to avoid macOS keychain
/// prompts. This fork ships its builds on the development channel, so the
/// default is the keychain and the file is opt-in, for throwaway testing.
///
/// Only works in development. Setting this environment variable in other
/// release channels is a no-op.
static ZED_DEVELOPMENT_USE_FILE_CREDENTIALS: LazyLock<bool> = LazyLock::new(|| {
    std::env::var("ZED_DEVELOPMENT_USE_FILE_CREDENTIALS").is_ok_and(|value| !value.is_empty())
});

/// The keychain name for the credential at `url`.
///
/// Upstream Zed stores each credential under its bare URL. This app puts its
/// URL scheme from `editions.json` in front (`forgesuper:https://…`), so its
/// entries stay apart from Zed's when both are installed. Credentials saved
/// under the bare URL by earlier builds are copied across when first read
/// (see [`KeychainCredentialsProvider`]).
fn keychain_key(url: &str) -> String {
    let scheme = app_names::PRODUCT.scheme;
    if scheme == "zed" {
        url.to_string()
    } else {
        format!("{scheme}:{url}")
    }
}

/// Set once the move of the old credentials file into the keychain has been
/// started, so it runs once per process however often [`global`] is called.
static MIGRATION_STARTED: AtomicBool = AtomicBool::new(false);

pub struct ZedCredentialsProvider(pub Arc<dyn CredentialsProvider>);

impl Global for ZedCredentialsProvider {}

/// Returns the global [`CredentialsProvider`].
pub fn init_global(cx: &mut App) {
    let provider = new(cx);
    cx.set_global(ZedCredentialsProvider(provider));
}

pub fn global(cx: &App) -> Arc<dyn CredentialsProvider> {
    cx.try_global::<ZedCredentialsProvider>()
        .map(|provider| provider.0.clone())
        .unwrap_or_else(|| new(cx))
}

fn new(cx: &App) -> Arc<dyn CredentialsProvider> {
    let use_development_provider = match ReleaseChannel::try_global(cx) {
        Some(ReleaseChannel::Dev) => *ZED_DEVELOPMENT_USE_FILE_CREDENTIALS,
        Some(ReleaseChannel::Nightly | ReleaseChannel::Preview | ReleaseChannel::Stable) | None => {
            false
        }
    };

    if use_development_provider {
        return Arc::new(DevelopmentCredentialsProvider::new());
    }

    let provider = Arc::new(KeychainCredentialsProvider {
        legacy_file: DevelopmentCredentialsProvider::new(),
    });
    if !MIGRATION_STARTED.swap(true, Ordering::SeqCst) {
        let provider = provider.clone();
        cx.spawn(async move |cx| provider.migrate_legacy_file(cx).await)
            .detach();
    }
    provider
}

/// A credentials provider that stores credentials in the system keychain,
/// under [`keychain_key`].
///
/// Earlier builds of this fork stored credentials in the development file
/// (see [`DevelopmentCredentialsProvider`]). Entries still in that file are
/// moved into the keychain: all of them once at startup, and any single one
/// as soon as it is read, so a read that runs before the startup move still
/// finds its credential. An entry leaves the file only after the keychain
/// has accepted it, and the file is deleted once it is empty.
///
/// Earlier builds also stored keychain entries under the bare URL. A read
/// that finds nothing under the new key copies the bare-URL entry across.
/// The bare-URL entry is left in place, since upstream Zed may use it too;
/// deleting a credential removes both, so a deleted one cannot come back.
struct KeychainCredentialsProvider {
    legacy_file: DevelopmentCredentialsProvider,
}

impl KeychainCredentialsProvider {
    /// Moves every entry of the old credentials file into the keychain.
    /// An entry the keychain already holds is dropped from the file, since
    /// the keychain's copy is the newer one.
    async fn migrate_legacy_file(&self, cx: &AsyncApp) {
        if !self.legacy_file.exists() {
            return;
        }
        let entries = match self.legacy_file.load_credentials() {
            Ok(entries) => entries,
            Err(error) => {
                log::error!("could not read the old credentials file: {error:#}");
                return;
            }
        };

        let mut moved = 0;
        for (url, (username, password)) in entries {
            let key = keychain_key(&url);
            let in_keychain = cx
                .update(|cx| cx.read_credentials(&key))
                .await
                .is_ok_and(|found| found.is_some());
            if !in_keychain {
                if let Err(error) = cx
                    .update(|cx| cx.write_credentials(&key, &username, &password))
                    .await
                {
                    log::error!(
                        "could not move the credential for {url} into the keychain: {error:#}"
                    );
                    continue;
                }
                moved += 1;
            }
            self.legacy_file.forget(&url);
        }
        if moved > 0 {
            log::info!(
                "moved {moved} credential(s) from the old credentials file into the keychain"
            );
        }
    }
}

impl CredentialsProvider for KeychainCredentialsProvider {
    fn read_credentials<'a>(
        &'a self,
        url: &'a str,
        cx: &'a AsyncApp,
    ) -> Pin<Box<dyn Future<Output = Result<Option<(String, Vec<u8>)>>> + 'a>> {
        async move {
            let key = keychain_key(url);
            if let Some(found) = cx.update(|cx| cx.read_credentials(&key)).await? {
                return Ok(Some(found));
            }
            if key != url
                && let Some((username, password)) =
                    cx.update(|cx| cx.read_credentials(url)).await?
            {
                if let Err(error) = cx
                    .update(|cx| cx.write_credentials(&key, &username, &password))
                    .await
                {
                    log::error!("could not copy the credential for {url} to {key}: {error:#}");
                }
                return Ok(Some((username, password)));
            }
            let Some((username, password)) = self.legacy_file.get(url) else {
                return Ok(None);
            };
            match cx
                .update(|cx| cx.write_credentials(&key, &username, &password))
                .await
            {
                Ok(()) => self.legacy_file.forget(url),
                Err(error) => {
                    log::error!(
                        "could not move the credential for {url} into the keychain: {error:#}"
                    )
                }
            }
            Ok(Some((username, password)))
        }
        .boxed_local()
    }

    fn write_credentials<'a>(
        &'a self,
        url: &'a str,
        username: &'a str,
        password: &'a [u8],
        cx: &'a AsyncApp,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
        async move {
            let key = keychain_key(url);
            cx.update(|cx| cx.write_credentials(&key, username, password))
                .await?;
            self.legacy_file.forget(url);
            Ok(())
        }
        .boxed_local()
    }

    #[cfg(target_os = "macos")]
    fn write_credentials_if_not_exists<'a>(
        &'a self,
        url: &'a str,
        username: &'a str,
        password: &'a [u8],
        cx: &'a AsyncApp,
    ) -> Pin<Box<dyn Future<Output = Result<bool>> + 'a>> {
        async move {
            cx.update(move |cx| cx.write_credentials_if_not_exists(url, username, password))
                .await
        }
        .boxed_local()
    }

    fn delete_credentials<'a>(
        &'a self,
        url: &'a str,
        cx: &'a AsyncApp,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
        async move {
            // Dropped from the old file first, so a deleted credential cannot
            // come back from it on the next read.
            self.legacy_file.forget(url);
            let key = keychain_key(url);
            if key != url {
                // Removed under the bare URL too, or the next read would copy
                // it back. Usually there is none, which Windows reports as an
                // error, so the result is ignored.
                cx.update(|cx| cx.delete_credentials(url)).await.ok();
            }
            cx.update(|cx| cx.delete_credentials(&key)).await
        }
        .boxed_local()
    }
}

/// A credentials provider that stores credentials in a local file.
///
/// This MUST only be used in development, as this is not a secure way of storing
/// credentials on user machines.
///
/// Its existence is purely to work around the annoyance of having to constantly
/// re-allow access to the system keychain when developing Zed.
struct DevelopmentCredentialsProvider {
    path: PathBuf,
}

impl DevelopmentCredentialsProvider {
    fn new() -> Self {
        let path = paths::config_dir().join("development_credentials");

        Self { path }
    }

    fn exists(&self) -> bool {
        self.path.exists()
    }

    fn load_credentials(&self) -> Result<HashMap<String, (String, Vec<u8>)>> {
        let json = std::fs::read(&self.path)?;
        let credentials: HashMap<String, (String, Vec<u8>)> = serde_json::from_slice(&json)?;

        Ok(credentials)
    }

    fn save_credentials(&self, credentials: &HashMap<String, (String, Vec<u8>)>) -> Result<()> {
        let json = serde_json::to_string(credentials)?;
        std::fs::write(&self.path, json)?;

        Ok(())
    }

    /// The entry for `url`, if the file exists and holds one.
    fn get(&self, url: &str) -> Option<(String, Vec<u8>)> {
        if !self.exists() {
            return None;
        }
        self.load_credentials().ok()?.remove(url)
    }

    /// Removes the entry for `url`, and the file itself once it is empty.
    fn forget(&self, url: &str) {
        if !self.exists() {
            return;
        }
        let Ok(mut credentials) = self.load_credentials() else {
            return;
        };
        if credentials.remove(url).is_none() && !credentials.is_empty() {
            return;
        }
        let result = if credentials.is_empty() {
            std::fs::remove_file(&self.path).map_err(anyhow::Error::from)
        } else {
            self.save_credentials(&credentials)
        };
        if let Err(error) = result {
            log::error!("could not update the old credentials file: {error:#}");
        }
    }
}

impl CredentialsProvider for DevelopmentCredentialsProvider {
    fn read_credentials<'a>(
        &'a self,
        url: &'a str,
        _cx: &'a AsyncApp,
    ) -> Pin<Box<dyn Future<Output = Result<Option<(String, Vec<u8>)>>> + 'a>> {
        async move {
            Ok(self
                .load_credentials()
                .unwrap_or_default()
                .get(url)
                .cloned())
        }
        .boxed_local()
    }

    fn write_credentials<'a>(
        &'a self,
        url: &'a str,
        username: &'a str,
        password: &'a [u8],
        _cx: &'a AsyncApp,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
        async move {
            let mut credentials = self.load_credentials().unwrap_or_default();
            credentials.insert(url.to_string(), (username.to_string(), password.to_vec()));

            self.save_credentials(&credentials)
        }
        .boxed_local()
    }

    fn delete_credentials<'a>(
        &'a self,
        url: &'a str,
        _cx: &'a AsyncApp,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
        async move {
            let mut credentials = self.load_credentials()?;
            credentials.remove(url);

            self.save_credentials(&credentials)
        }
        .boxed_local()
    }
}
