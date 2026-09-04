//! API keys: an in-memory map in front of one persisted secret.
//!
//! Keys are keyed by provider id (`ProviderId::as_str`) and persisted together as a single JSON
//! map through GPUI's credentials
//! API — one item (`majik://api-keys`) in the macOS keychain / Windows Credential Manager / Linux
//! Secret Service. One item rather than one per provider means one read at startup, one write per
//! save, and at most one keychain dialog.
//!
//! Debug builds keep the map in `development_credentials.json` instead, as Zed does. A
//! login-keychain item's ACL trusts the *code signature* of the binary that created it, and an
//! unsigned `cargo run` binary or an ad-hoc-signed bundle changes identity on every rebuild, so
//! each read after a rebuild would raise the "enter your keychain password" dialog.
//! `MAJIK_USE_KEYCHAIN=1` opts a debug build back into the keychain.
//!
//! The store is deliberately *not* split by build channel ([`crate::config::credentials_dir`] is
//! always the stable folder, and [`KEYCHAIN_URL`] carries no channel), so a provider key is entered
//! once and survives wiping the dev folder. The cost is that the map is one item, so removing a key
//! in one channel removes it in the other.

use anyhow::{Context as _, Result};
use gpui::{App, AppContext as _, AsyncApp, Task};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

pub type KeyMap = HashMap<String, String>;

/// Where the whole key map is persisted. Every backend stores the map as one unit.
pub trait SecretBackend: Send + Sync {
    fn read(&self, cx: &mut App) -> Task<Result<KeyMap>>;
    fn write(&self, keys: KeyMap, cx: &mut App) -> Task<Result<()>>;
}

/// The app's API keys. `get` is synchronous and never does IO (the generation engine calls it from
/// its own threads); `set` / `delete` update the cache at once and persist in the background,
/// rolling the cache back if persisting fails.
///
/// The startup [`Self::load`] can take a while (the keychain may sit behind a dialog) and the app
/// stays usable meanwhile, so an edit made before it finishes neither replaces the stored map with
/// the one key the user just typed nor gets overwritten when the read finally resolves.
pub struct ApiKeys {
    cache: Mutex<KeyMap>,
    backend: Box<dyn SecretBackend>,
    /// Whether the startup read has reached the cache. Until then the cache holds only what was
    /// edited this session, so a save writes through the stored map instead of replacing it.
    loaded: AtomicBool,
    /// Providers edited before the startup read finished; their edit wins over the read's snapshot.
    edited_before_load: Mutex<HashSet<String>>,
}

impl ApiKeys {
    pub fn new(backend: Box<dyn SecretBackend>) -> Self {
        Self { cache: Mutex::new(KeyMap::new()), backend, loaded: AtomicBool::new(false), edited_before_load: Mutex::new(HashSet::new()) }
    }

    /// Keys that live only in memory (tests, `MAJIK_MOCK_KEYS`).
    pub fn in_memory<'a>(seed: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let keys: KeyMap = seed.into_iter().map(|(provider, key)| (provider.to_string(), key.to_string())).collect();
        Self {
            cache: Mutex::new(keys.clone()),
            backend: Box::new(MemoryBackend { keys: Mutex::new(keys) }),
            loaded: AtomicBool::new(true),
            edited_before_load: Mutex::new(HashSet::new()),
        }
    }

    /// The store for this process: `MAJIK_MOCK_KEYS` → in-memory Mock key; debug builds → the local
    /// development file unless `MAJIK_USE_KEYCHAIN` is set; otherwise the OS keychain.
    pub fn for_environment() -> Self {
        let choice = choose_backend(
            std::env::var_os("MAJIK_MOCK_KEYS").is_some(),
            cfg!(debug_assertions),
            std::env::var_os("MAJIK_USE_KEYCHAIN").is_some(),
        );
        match choice {
            Backend::Memory => Self::in_memory([("Mock", "mock")]),
            Backend::File => match crate::config::credentials_dir() {
                Some(dir) => Self::new(Box::new(FileBackend { path: dir.join(DEVELOPMENT_FILE) })),
                None => Self::new(Box::new(KeychainBackend)),
            },
            Backend::Keychain => Self::new(Box::new(KeychainBackend)),
        }
    }

    pub fn get(&self, provider: &str) -> Option<String> {
        self.cache().get(provider).cloned()
    }

    /// Fill the cache from the backend. Call once at startup. Keys edited while the read was
    /// pending keep their edited value.
    pub fn load(self: &Arc<Self>, cx: &mut App) -> Task<Result<()>> {
        let read = self.backend.read(cx);
        let this = self.clone();
        cx.spawn(async move |_cx| {
            let keys = read.await?;
            let edited = std::mem::take(&mut *this.edited_before_load());
            let mut cache = this.cache();
            for (provider, key) in keys {
                if !edited.contains(&provider) {
                    cache.insert(provider, key);
                }
            }
            this.loaded.store(true, Ordering::SeqCst);
            Ok(())
        })
    }

    /// Store `key` (trimmed) for `provider`. A blank key is a no-op.
    pub fn set(self: &Arc<Self>, provider: &str, key: &str, cx: &mut App) -> Task<Result<()>> {
        let key = key.trim();
        if key.is_empty() {
            return Task::ready(Ok(()));
        }
        let previous = self.cache().insert(provider.to_string(), key.to_string());
        self.note_edit(provider);
        // The provider, never the key.
        majik_telemetry::event!("Provider Key Added", provider);
        self.persist(provider, previous, cx)
    }

    pub fn delete(self: &Arc<Self>, provider: &str, cx: &mut App) -> Task<Result<()>> {
        let previous = self.cache().remove(provider);
        self.note_edit(provider);
        majik_telemetry::event!("Provider Key Removed", provider);
        self.persist(provider, previous, cx)
    }

    fn note_edit(&self, provider: &str) {
        if !self.loaded.load(Ordering::SeqCst) {
            self.edited_before_load().insert(provider.to_string());
        }
    }

    fn persist(self: &Arc<Self>, provider: &str, previous: Option<String>, cx: &mut App) -> Task<Result<()>> {
        // Before the startup read finishes the cache lacks whatever it will bring, so writing the
        // cache alone would wipe every other provider's key: write through the stored map. Both are
        // captured now, because the startup read may finish (and clear the edit list) before the
        // write below runs, and the stored map must still lose what this session deleted.
        let (stored, edited) = if self.loaded.load(Ordering::SeqCst) {
            (None, Vec::new())
        } else {
            (Some(self.backend.read(cx)), self.edited_before_load().iter().cloned().collect())
        };
        let this = self.clone();
        let provider = provider.to_string();
        cx.spawn(async move |cx| {
            let result = this.write_through(stored, edited, cx).await;
            if result.is_err() {
                let mut cache = this.cache();
                match previous {
                    Some(key) => {
                        cache.insert(provider, key);
                    }
                    None => {
                        cache.remove(&provider);
                        this.edited_before_load().remove(&provider);
                    }
                }
            }
            result
        })
    }

    /// Write the cache over `stored` (the backend's map when the startup read hasn't finished yet;
    /// `None` once the cache holds everything), dropping the `edited` providers the cache no longer
    /// holds, which are the deletions made this session.
    async fn write_through(&self, stored: Option<Task<Result<KeyMap>>>, edited: Vec<String>, cx: &mut AsyncApp) -> Result<()> {
        let mut keys = match stored {
            Some(read) => read.await?,
            None => KeyMap::new(),
        };
        {
            let cache = self.cache();
            for provider in edited {
                if !cache.contains_key(&provider) {
                    keys.remove(&provider);
                }
            }
            keys.extend(cache.iter().map(|(provider, key)| (provider.clone(), key.clone())));
        }
        cx.update(|cx| self.backend.write(keys, cx)).await
    }

    fn cache(&self) -> MutexGuard<'_, KeyMap> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn edited_before_load(&self) -> MutexGuard<'_, HashSet<String>> {
        self.edited_before_load.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Memory,
    File,
    Keychain,
}

pub fn choose_backend(mock_keys: bool, debug: bool, force_keychain: bool) -> Backend {
    if mock_keys {
        Backend::Memory
    } else if debug && !force_keychain {
        Backend::File
    } else {
        Backend::Keychain
    }
}

pub struct MemoryBackend {
    keys: Mutex<KeyMap>,
}

impl SecretBackend for MemoryBackend {
    fn read(&self, _cx: &mut App) -> Task<Result<KeyMap>> {
        Task::ready(Ok(self.keys.lock().unwrap_or_else(PoisonError::into_inner).clone()))
    }

    fn write(&self, keys: KeyMap, _cx: &mut App) -> Task<Result<()>> {
        *self.keys.lock().unwrap_or_else(PoisonError::into_inner) = keys;
        Task::ready(Ok(()))
    }
}

pub const DEVELOPMENT_FILE: &str = "development_credentials.json";

/// Plain JSON on disk, readable only by the user. Debug builds only; see the module docs.
pub struct FileBackend {
    pub path: PathBuf,
}

impl SecretBackend for FileBackend {
    fn read(&self, cx: &mut App) -> Task<Result<KeyMap>> {
        let path = self.path.clone();
        cx.background_spawn(async move {
            match std::fs::read(&path) {
                Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display())),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(KeyMap::new()),
                Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
            }
        })
    }

    fn write(&self, keys: KeyMap, cx: &mut App) -> Task<Result<()>> {
        let path = self.path.clone();
        cx.background_spawn(async move {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
            }
            let json = serde_json::to_vec_pretty(&keys)?;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            // Owner-only on unix; on Windows the config dir (`%APPDATA%`) is already private to the
            // user's account, so the default ACL is the equivalent.
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            let mut file = options.open(&path).with_context(|| format!("opening {}", path.display()))?;
            std::io::Write::write_all(&mut file, &json).with_context(|| format!("writing {}", path.display()))?;
            Ok(())
        })
    }
}

/// GPUI's platform credential store; one internet-password-style item keyed by [`KEYCHAIN_URL`].
pub struct KeychainBackend;

pub const KEYCHAIN_URL: &str = "majik://api-keys";
const KEYCHAIN_USERNAME: &str = "majik";

impl SecretBackend for KeychainBackend {
    fn read(&self, cx: &mut App) -> Task<Result<KeyMap>> {
        let read = cx.read_credentials(KEYCHAIN_URL);
        cx.spawn(async move |_cx| match read.await.context("reading API keys from the keychain")? {
            // Missing, or the user declined the keychain dialog: behave as "no keys".
            None => Ok(KeyMap::new()),
            Some((_, bytes)) => serde_json::from_slice(&bytes).context("parsing the keychain API-key item"),
        })
    }

    fn write(&self, keys: KeyMap, cx: &mut App) -> Task<Result<()>> {
        if keys.is_empty() {
            return cx.delete_credentials(KEYCHAIN_URL);
        }
        match serde_json::to_vec(&keys) {
            Ok(json) => cx.write_credentials(KEYCHAIN_URL, KEYCHAIN_USERNAME, &json),
            Err(e) => Task::ready(Err(e.into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestBackend;
    use gpui::TestAppContext;

    fn keys_over(backend: &TestBackend) -> Arc<ApiKeys> {
        Arc::new(ApiKeys::new(Box::new(backend.clone())))
    }

    #[gpui::test]
    fn get_before_load_is_none(cx: &mut TestAppContext) {
        let backend = TestBackend::with([("fal.ai", "k")]);
        let keys = keys_over(&backend);
        assert!(keys.get("fal.ai").is_none());
        let _ = cx;
    }

    #[gpui::test(iterations = 20)]
    async fn load_fills_cache_from_backend(cx: &mut TestAppContext) {
        let backend = TestBackend::with([("fal.ai", "k1"), ("Replicate", "k2")]);
        let keys = keys_over(&backend);
        cx.update(|cx| keys.load(cx)).await.unwrap();
        assert_eq!(keys.get("fal.ai").as_deref(), Some("k1"));
        assert_eq!(keys.get("Replicate").as_deref(), Some("k2"));
        assert!(keys.get("Mock").is_none());
    }

    #[gpui::test]
    async fn load_error_leaves_cache_untouched(cx: &mut TestAppContext) {
        let backend = TestBackend::with([("fal.ai", "on-disk")]);
        let keys = keys_over(&backend);
        cx.update(|cx| keys.set("fal.ai", "in-memory", cx)).await.unwrap();
        backend.fail_reads(true);
        let err = cx.update(|cx| keys.load(cx)).await.unwrap_err();
        assert!(err.to_string().contains("read failed"), "{err:#}");
        assert_eq!(keys.get("fal.ai").as_deref(), Some("in-memory"));
    }

    #[gpui::test(iterations = 20)]
    async fn set_while_the_startup_read_is_pending_keeps_stored_keys(cx: &mut TestAppContext) {
        let backend = TestBackend::with([("fal.ai", "stored")]);
        backend.slow_reads(true);
        let keys = keys_over(&backend);
        let load = cx.update(|cx| keys.load(cx));
        // Settings saves a key while the keychain dialog is still up.
        cx.update(|cx| keys.set("Replicate", "typed", cx)).await.unwrap();
        assert_eq!(backend.get("fal.ai").as_deref(), Some("stored"), "the save wrote through the stored map");
        assert_eq!(backend.get("Replicate").as_deref(), Some("typed"));
        load.await.unwrap();
        assert_eq!(keys.get("fal.ai").as_deref(), Some("stored"));
        assert_eq!(keys.get("Replicate").as_deref(), Some("typed"), "the typed key survives the startup read");
    }

    #[gpui::test(iterations = 20)]
    async fn delete_while_the_startup_read_is_pending_stays_deleted(cx: &mut TestAppContext) {
        let backend = TestBackend::with([("fal.ai", "a"), ("Replicate", "b")]);
        backend.slow_reads(true);
        let keys = keys_over(&backend);
        let load = cx.update(|cx| keys.load(cx));
        cx.update(|cx| keys.delete("fal.ai", cx)).await.unwrap();
        load.await.unwrap();
        assert!(keys.get("fal.ai").is_none(), "the read must not resurrect it");
        assert_eq!(keys.get("Replicate").as_deref(), Some("b"));
        assert!(backend.get("fal.ai").is_none());
        assert_eq!(backend.get("Replicate").as_deref(), Some("b"));
    }

    #[gpui::test]
    async fn set_before_load_with_an_unreadable_store_fails_and_rolls_back(cx: &mut TestAppContext) {
        let backend = TestBackend::with([("fal.ai", "stored")]);
        backend.fail_reads(true);
        let keys = keys_over(&backend);
        let err = cx.update(|cx| keys.set("Replicate", "typed", cx)).await.unwrap_err();
        assert!(err.to_string().contains("read failed"), "{err:#}");
        assert!(keys.get("Replicate").is_none(), "cache rolled back");
        assert_eq!(backend.get("fal.ai").as_deref(), Some("stored"), "nothing was overwritten");
        assert_eq!(backend.writes(), 0);
    }

    #[gpui::test(iterations = 20)]
    async fn set_then_get(cx: &mut TestAppContext) {
        let backend = TestBackend::default();
        let keys = keys_over(&backend);
        let task = cx.update(|cx| keys.set("fal.ai", "secret", cx));
        assert_eq!(keys.get("fal.ai").as_deref(), Some("secret"), "cache updates before the write lands");
        task.await.unwrap();
        assert_eq!(keys.get("fal.ai").as_deref(), Some("secret"));
    }

    #[gpui::test]
    async fn set_trims_key(cx: &mut TestAppContext) {
        let backend = TestBackend::default();
        let keys = keys_over(&backend);
        cx.update(|cx| keys.set("fal.ai", "  sk-1 \n", cx)).await.unwrap();
        assert_eq!(keys.get("fal.ai").as_deref(), Some("sk-1"));
        assert_eq!(backend.get("fal.ai").as_deref(), Some("sk-1"));
    }

    #[gpui::test]
    async fn blank_key_is_noop(cx: &mut TestAppContext) {
        let backend = TestBackend::default();
        let keys = keys_over(&backend);
        cx.update(|cx| keys.set("fal.ai", "   ", cx)).await.unwrap();
        assert!(keys.get("fal.ai").is_none());
        assert_eq!(backend.writes(), 0);
    }

    #[gpui::test]
    async fn set_persists_whole_map(cx: &mut TestAppContext) {
        let backend = TestBackend::default();
        let keys = keys_over(&backend);
        cx.update(|cx| keys.set("fal.ai", "a", cx)).await.unwrap();
        cx.update(|cx| keys.set("Replicate", "b", cx)).await.unwrap();
        assert_eq!(backend.get("fal.ai").as_deref(), Some("a"));
        assert_eq!(backend.get("Replicate").as_deref(), Some("b"));
        assert_eq!(backend.writes(), 2);
    }

    #[gpui::test(iterations = 20)]
    async fn delete_removes_and_persists(cx: &mut TestAppContext) {
        let backend = TestBackend::with([("fal.ai", "a"), ("Replicate", "b")]);
        let keys = keys_over(&backend);
        cx.update(|cx| keys.load(cx)).await.unwrap();
        cx.update(|cx| keys.delete("fal.ai", cx)).await.unwrap();
        assert!(keys.get("fal.ai").is_none());
        assert!(backend.get("fal.ai").is_none());
        assert_eq!(backend.get("Replicate").as_deref(), Some("b"), "other keys survive");
    }

    #[gpui::test]
    async fn delete_last_key_leaves_empty_map(cx: &mut TestAppContext) {
        let backend = TestBackend::with([("fal.ai", "a")]);
        let keys = keys_over(&backend);
        cx.update(|cx| keys.load(cx)).await.unwrap();
        cx.update(|cx| keys.delete("fal.ai", cx)).await.unwrap();
        assert!(backend.snapshot().is_empty());
    }

    #[gpui::test]
    async fn delete_missing_key_is_ok(cx: &mut TestAppContext) {
        let backend = TestBackend::default();
        let keys = keys_over(&backend);
        cx.update(|cx| keys.delete("fal.ai", cx)).await.unwrap();
    }

    #[gpui::test(iterations = 20)]
    async fn failed_write_reverts_cache_and_returns_error(cx: &mut TestAppContext) {
        let backend = TestBackend::with([("fal.ai", "old")]);
        let keys = keys_over(&backend);
        cx.update(|cx| keys.load(cx)).await.unwrap();
        backend.fail_writes(true);
        let err = cx.update(|cx| keys.set("fal.ai", "new", cx)).await.unwrap_err();
        assert!(err.to_string().contains("write failed"), "{err:#}");
        assert_eq!(keys.get("fal.ai").as_deref(), Some("old"), "cache rolled back");
        assert_eq!(backend.get("fal.ai").as_deref(), Some("old"));
    }

    #[gpui::test]
    async fn failed_write_of_new_key_removes_it_from_cache(cx: &mut TestAppContext) {
        let backend = TestBackend::default();
        backend.fail_writes(true);
        let keys = keys_over(&backend);
        cx.update(|cx| keys.set("fal.ai", "new", cx)).await.unwrap_err();
        assert!(keys.get("fal.ai").is_none());
    }

    #[gpui::test]
    async fn failed_delete_keeps_key(cx: &mut TestAppContext) {
        let backend = TestBackend::with([("fal.ai", "a")]);
        let keys = keys_over(&backend);
        cx.update(|cx| keys.load(cx)).await.unwrap();
        backend.fail_writes(true);
        cx.update(|cx| keys.delete("fal.ai", cx)).await.unwrap_err();
        assert_eq!(keys.get("fal.ai").as_deref(), Some("a"));
        assert_eq!(backend.get("fal.ai").as_deref(), Some("a"));
    }

    #[gpui::test]
    fn in_memory_is_seeded(cx: &mut TestAppContext) {
        let keys = ApiKeys::in_memory([("Mock", "mock")]);
        assert_eq!(keys.get("Mock").as_deref(), Some("mock"));
        assert!(keys.get("fal.ai").is_none());
        let _ = cx;
    }

    #[gpui::test]
    async fn file_backend_round_trips_across_instances(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(DEVELOPMENT_FILE);
        let keys = Arc::new(ApiKeys::new(Box::new(FileBackend { path: path.clone() })));
        cx.update(|cx| keys.set("fal.ai", "sk-file", cx)).await.unwrap();
        cx.update(|cx| keys.set("Replicate", "r8-file", cx)).await.unwrap();

        let reopened = Arc::new(ApiKeys::new(Box::new(FileBackend { path })));
        cx.update(|cx| reopened.load(cx)).await.unwrap();
        assert_eq!(reopened.get("fal.ai").as_deref(), Some("sk-file"));
        assert_eq!(reopened.get("Replicate").as_deref(), Some("r8-file"));
    }

    #[gpui::test]
    async fn file_backend_missing_file_reads_empty(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let backend = FileBackend { path: dir.path().join("missing.json") };
        let keys = cx.update(|cx| backend.read(cx)).await.unwrap();
        assert!(keys.is_empty());
    }

    #[gpui::test]
    async fn file_backend_corrupt_file_is_error(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, b"not json").unwrap();
        let backend = FileBackend { path };
        let err = cx.update(|cx| backend.read(cx)).await.unwrap_err();
        assert!(err.to_string().contains("parsing"), "{err:#}");
    }

    #[cfg(unix)]
    #[gpui::test]
    async fn file_backend_file_is_private(cx: &mut TestAppContext) {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DEVELOPMENT_FILE);
        let backend = FileBackend { path: path.clone() };
        cx.update(|cx| backend.write(KeyMap::from([("fal.ai".to_string(), "k".to_string())]), cx)).await.unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[gpui::test]
    async fn keychain_backend_reads_empty_and_writes_on_the_test_platform(cx: &mut TestAppContext) {
        let keys = cx.update(|cx| KeychainBackend.read(cx)).await.unwrap();
        assert!(keys.is_empty());
        cx.update(|cx| KeychainBackend.write(KeyMap::from([("fal.ai".to_string(), "k".to_string())]), cx)).await.unwrap();
        cx.update(|cx| KeychainBackend.write(KeyMap::new(), cx)).await.unwrap();
    }

    #[test]
    fn backend_choice() {
        assert_eq!(choose_backend(true, true, true), Backend::Memory, "mock keys win over everything");
        assert_eq!(choose_backend(true, false, false), Backend::Memory);
        assert_eq!(choose_backend(false, true, false), Backend::File, "debug builds stay out of the keychain");
        assert_eq!(choose_backend(false, true, true), Backend::Keychain, "unless forced");
        assert_eq!(choose_backend(false, false, false), Backend::Keychain, "release builds use the keychain");
        assert_eq!(choose_backend(false, false, true), Backend::Keychain);
    }
}
