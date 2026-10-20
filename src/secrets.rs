//! API keys: env vars first, then `~/.s1/auth.json` (pi `auth.json` shape).
//!
//! Rules (spec "Secrets"):
//! - The file must not be readable or writable by group or others; s1 refuses it otherwise.
//! - The directory is made with mode 0700, the file with 0600.
//! - Writes go to a temporary file and then an atomic rename, under a lock.
//! - Key values are never printed or logged: [`Secret`] redacts itself in `Debug`.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::paths::Paths;

/// Name of the auth entry that holds the local bearer token for TCP clients.
pub const LOCAL_TOKEN_ENTRY: &str = "s1-local";

/// A secret string. `Debug` and `Display` never show the value.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    /// The raw value. Use only to build an outgoing request header.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SecretsError {
    #[error(
        "{path} has mode {mode:o}; group or others can access it. Run `chmod 600 {path}` and try again"
    )]
    InsecureMode { path: PathBuf, mode: u32 },
    #[error("{path}: {reason}; s1 refuses to use it")]
    Insecure { path: PathBuf, reason: String },
    #[error("cannot read {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} is not valid JSON: {source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("{path} must hold a JSON object")]
    NotObject { path: PathBuf },
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> SecretsError + '_ {
    move |source| SecretsError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Where a key came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    Env,
    File,
}

impl KeySource {
    pub fn as_str(self) -> &'static str {
        match self {
            KeySource::Env => "env",
            KeySource::File => "file",
        }
    }
}

/// A resolved provider credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKey {
    pub key: Secret,
    /// Cloudflare account id (`accountId` in the file).
    pub account_id: Option<String>,
    pub source: KeySource,
}

/// Env vars that override the file, per provider: (key var, account id var).
pub fn env_vars(provider: &str) -> (Option<&'static str>, Option<&'static str>) {
    match provider {
        "typesafe" => (Some("TYPESAFE_API_KEY"), None),
        "workers-ai" => (Some("CLOUDFLARE_API_TOKEN"), Some("CLOUDFLARE_ACCOUNT_ID")),
        "openai" => (Some("OPENAI_API_KEY"), None),
        "tsfm" => (Some("TSFM_API_KEY"), None),
        _ => (None, None),
    }
}

/// The parsed `auth.json`. Unknown entries and fields are kept on write.
#[derive(Clone, Default)]
pub struct AuthFile {
    entries: Map<String, Value>,
}

impl fmt::Debug for AuthFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthFile")
            .field("entries", &self.entries.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// The real uid of this process.
pub fn current_uid() -> u32 {
    // SAFETY: getuid(2) cannot fail and has no preconditions.
    unsafe { libc::getuid() }
}

fn insecure(path: &Path, reason: impl Into<String>) -> SecretsError {
    SecretsError::Insecure {
        path: path.to_path_buf(),
        reason: reason.into(),
    }
}

/// Metadata of a directory owned by this user; `None` if it does not exist.
fn check_owned_dir(dir: &Path) -> Result<Option<fs::Metadata>, SecretsError> {
    use std::os::unix::fs::MetadataExt;
    let meta = match fs::metadata(dir) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_err(dir)(e)),
    };
    if !meta.is_dir() {
        return Err(insecure(dir, "not a directory"));
    }
    if meta.uid() != current_uid() {
        return Err(insecure(dir, "owned by another user"));
    }
    Ok(Some(meta))
}

/// The directory that holds the auth file must be a directory owned by this
/// user that group and others cannot access, so nobody else can swap the file.
/// `Ok(false)` if it does not exist. The socket dir follows the same rule.
pub fn check_private_dir(dir: &Path) -> Result<bool, SecretsError> {
    use std::os::unix::fs::MetadataExt;
    let Some(meta) = check_owned_dir(dir)? else {
        return Ok(false);
    };
    let mode = meta.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(insecure(
            dir,
            format!(
                "directory has mode {mode:o}; group or others can access it. Run `chmod 700 {}` and try again",
                dir.display()
            ),
        ));
    }
    Ok(true)
}

impl AuthFile {
    /// Read the file. A missing file is empty. An insecure mode, owner, or
    /// directory is an error; a symlink is refused.
    pub fn load(path: &Path) -> Result<Self, SecretsError> {
        use std::os::unix::fs::MetadataExt;
        let dir = path.parent().unwrap_or(Path::new("."));
        if !check_private_dir(dir)? {
            return Ok(Self::default());
        }
        let mut file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
                return Err(insecure(path, "is a symlink"));
            }
            Err(e) => return Err(io_err(path)(e)),
        };
        let meta = file.metadata().map_err(io_err(path))?;
        if !meta.is_file() {
            return Err(insecure(path, "not a regular file"));
        }
        if meta.uid() != current_uid() {
            return Err(insecure(path, "owned by another user"));
        }
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(SecretsError::InsecureMode {
                path: path.to_path_buf(),
                mode,
            });
        }
        let mut text = String::new();
        file.read_to_string(&mut text).map_err(io_err(path))?;
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        let value: Value = serde_json::from_str(&text).map_err(|source| SecretsError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        match value {
            Value::Object(entries) => Ok(Self { entries }),
            _ => Err(SecretsError::NotObject {
                path: path.to_path_buf(),
            }),
        }
    }

    /// Write atomically: temp file (0600) in the same dir, fsync, rename.
    pub fn save(&self, path: &Path) -> Result<(), SecretsError> {
        let dir = path.parent().unwrap_or(Path::new("."));
        ensure_private_dir(dir)?;
        let mut text = serde_json::to_string_pretty(&Value::Object(self.entries.clone()))
            .expect("JSON object serializes");
        text.push('\n');
        let tmp = dir.join(format!(
            ".{}.{}.tmp",
            path.file_name().and_then(|n| n.to_str()).unwrap_or("auth"),
            random_hex(8)
        ));
        let result = (|| {
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)
                .map_err(io_err(&tmp))?;
            f.write_all(text.as_bytes()).map_err(io_err(&tmp))?;
            f.sync_all().map_err(io_err(&tmp))?;
            fs::rename(&tmp, path).map_err(io_err(path))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }

    /// Key and account id stored for a provider (`type: api_key` only).
    pub fn api_key(&self, provider: &str) -> Option<(Secret, Option<String>)> {
        let entry = self.entries.get(provider)?.as_object()?;
        let ty = entry
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("api_key");
        if ty != "api_key" {
            return None;
        }
        let key = entry.get("key")?.as_str().filter(|k| !k.is_empty())?;
        let account = entry
            .get("accountId")
            .and_then(Value::as_str)
            .map(str::to_string);
        Some((Secret::new(key), account))
    }

    /// Set (or replace) a provider's API key. Other fields of the entry stay.
    pub fn set_api_key(&mut self, provider: &str, key: &Secret, account_id: Option<&str>) {
        let entry = self
            .entries
            .entry(provider.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !entry.is_object() {
            *entry = Value::Object(Map::new());
        }
        let obj = entry.as_object_mut().expect("object");
        obj.insert("type".into(), Value::String("api_key".into()));
        obj.insert("key".into(), Value::String(key.expose().to_string()));
        if let Some(acc) = account_id {
            obj.insert("accountId".into(), Value::String(acc.to_string()));
        }
    }

    /// Provider names and entry types. Never values.
    pub fn entries(&self) -> Vec<(String, String)> {
        self.entries
            .iter()
            .map(|(k, v)| {
                let ty = v
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("api_key")
                    .to_string();
                (k.clone(), ty)
            })
            .collect()
    }
}

/// Make the directory (mode 0700) if it is missing, then check it as
/// [`check_private_dir`] does. Writers set an existing directory of this
/// user back to 0700; readers only refuse it.
pub fn ensure_private_dir(dir: &Path) -> Result<(), SecretsError> {
    use std::os::unix::fs::DirBuilderExt;
    if !dir.exists() {
        if let Some(parent) = dir.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(io_err(parent))?;
        }
        match fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(io_err(dir)(e)),
        }
    }
    check_owned_dir(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(io_err(dir))?;
    check_private_dir(dir).map(|_| ())
}

/// A file this user made in a private dir (the daemon socket or its lock):
/// not a symlink and owned by this user. `Ok(false)` if it does not exist.
pub fn check_own_file(path: &Path) -> Result<bool, SecretsError> {
    use std::os::unix::fs::MetadataExt;
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(io_err(path)(e)),
    };
    if meta.file_type().is_symlink() {
        return Err(insecure(path, "is a symlink"));
    }
    if meta.uid() != current_uid() {
        return Err(insecure(path, "owned by another user"));
    }
    Ok(true)
}

/// Hex from the OS random source.
pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .expect("read /dev/urandom");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Open (or make) a lock file in a private dir without blocking: O_NOFOLLOW
/// refuses a symlink, O_NONBLOCK keeps a FIFO with no reader from hanging the
/// open, and the fstat check then refuses anything that is not a regular file
/// owned by this user, before any flock.
pub fn open_lock_file(lock_path: &Path) -> Result<File, SecretsError> {
    use std::os::unix::fs::MetadataExt;
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(lock_path)
        .map_err(|e| match e.raw_os_error() {
            Some(libc::ELOOP) => insecure(lock_path, "is a symlink"),
            // A FIFO with no reader.
            Some(libc::ENXIO) => insecure(lock_path, "not a regular file"),
            _ => io_err(lock_path)(e),
        })?;
    let meta = f.metadata().map_err(io_err(lock_path))?;
    if !meta.is_file() {
        return Err(insecure(lock_path, "not a regular file"));
    }
    if meta.uid() != current_uid() {
        return Err(insecure(lock_path, "owned by another user"));
    }
    Ok(f)
}

/// Exclusive advisory lock on `<auth>.lock` for read-modify-write.
struct Lock(File);

impl Lock {
    fn acquire(auth_path: &Path) -> Result<Self, SecretsError> {
        let dir = auth_path.parent().unwrap_or(Path::new("."));
        ensure_private_dir(dir)?;
        let lock_path = auth_path.with_extension("json.lock");
        let f = open_lock_file(&lock_path)?;
        use std::os::fd::AsRawFd;
        // SAFETY: flock on a valid, owned file descriptor.
        let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            return Err(io_err(&lock_path)(std::io::Error::last_os_error()));
        }
        Ok(Self(f))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // SAFETY: unlock the descriptor we locked.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

type EnvLookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// One provider's key status for `s1 auth list`. Holds no values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyStatus {
    pub provider: String,
    pub source: KeySource,
    pub entry_type: String,
}

/// Resolves keys from env vars, then the auth file. Reads the file on each
/// call, so `s1 auth set` takes effect without a daemon restart.
#[derive(Clone)]
pub struct KeyResolver {
    auth_path: PathBuf,
    env: EnvLookup,
}

impl fmt::Debug for KeyResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyResolver")
            .field("auth_path", &self.auth_path)
            .finish_non_exhaustive()
    }
}

impl KeyResolver {
    pub fn new(
        auth_path: PathBuf,
        env: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            auth_path,
            env: Arc::new(env),
        }
    }

    /// Use the process environment.
    pub fn from_process(paths: &Paths) -> Self {
        Self::new(paths.auth_file(), |k| std::env::var(k).ok())
    }

    pub fn auth_path(&self) -> &Path {
        &self.auth_path
    }

    fn env(&self, var: Option<&str>) -> Option<String> {
        var.and_then(|v| (self.env)(v)).filter(|v| !v.is_empty())
    }

    /// Key for a provider: env first, then file. `Ok(None)` if neither has one.
    pub fn resolve(&self, provider: &str) -> Result<Option<ApiKey>, SecretsError> {
        let (key_var, account_var) = env_vars(provider);
        let env_account = self.env(account_var);
        if let Some(key) = self.env(key_var) {
            let account_id = match env_account {
                Some(a) => Some(a),
                None if account_var.is_some() => AuthFile::load(&self.auth_path)?
                    .api_key(provider)
                    .and_then(|(_, a)| a),
                None => None,
            };
            return Ok(Some(ApiKey {
                key: Secret::new(key),
                account_id,
                source: KeySource::Env,
            }));
        }
        let file = AuthFile::load(&self.auth_path)?;
        Ok(file.api_key(provider).map(|(key, account)| ApiKey {
            key,
            account_id: env_account.or(account),
            source: KeySource::File,
        }))
    }

    /// Lock, load, change, save.
    pub fn update(&self, f: impl FnOnce(&mut AuthFile)) -> Result<(), SecretsError> {
        let _lock = Lock::acquire(&self.auth_path)?;
        let mut file = AuthFile::load(&self.auth_path)?;
        f(&mut file);
        file.save(&self.auth_path)
    }

    /// The local bearer token; made (and saved) if missing.
    pub fn ensure_local_token(&self) -> Result<Secret, SecretsError> {
        if let Some((key, _)) = AuthFile::load(&self.auth_path)?.api_key(LOCAL_TOKEN_ENTRY) {
            return Ok(key);
        }
        let _lock = Lock::acquire(&self.auth_path)?;
        let mut file = AuthFile::load(&self.auth_path)?;
        if let Some((key, _)) = file.api_key(LOCAL_TOKEN_ENTRY) {
            return Ok(key);
        }
        let token = Secret::new(format!("s1_{}", random_hex(32)));
        file.set_api_key(LOCAL_TOKEN_ENTRY, &token, None);
        file.save(&self.auth_path)?;
        Ok(token)
    }

    /// Which providers have keys, and where from. Never values.
    pub fn list(&self) -> Result<Vec<KeyStatus>, SecretsError> {
        let file = AuthFile::load(&self.auth_path)?;
        let mut out: BTreeMap<String, KeyStatus> = BTreeMap::new();
        for (provider, entry_type) in file.entries() {
            out.insert(
                provider.clone(),
                KeyStatus {
                    provider,
                    source: KeySource::File,
                    entry_type,
                },
            );
        }
        for provider in ["typesafe", "workers-ai", "openai"] {
            if self.env(env_vars(provider).0).is_some() {
                out.insert(
                    provider.to_string(),
                    KeyStatus {
                        provider: provider.to_string(),
                        source: KeySource::Env,
                        entry_type: "api_key".into(),
                    },
                );
            }
        }
        Ok(out.into_values().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn resolver(dir: &Path, env: &[(&str, &str)]) -> KeyResolver {
        let m: HashMap<String, String> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        KeyResolver::new(dir.join("s1/auth.json"), move |k| m.get(k).cloned())
    }

    fn write_auth(path: &Path, text: &str, mode: u32) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(path, text).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn missing_file_has_no_keys() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(resolver(dir.path(), &[]).resolve("typesafe").unwrap(), None);
    }

    #[test]
    fn reads_pi_shape() {
        let dir = tempfile::tempdir().unwrap();
        let r = resolver(dir.path(), &[]);
        write_auth(
            r.auth_path(),
            r#"{"typesafe":{"type":"api_key","key":"ts-file"},
               "workers-ai":{"type":"api_key","key":"cf-file","accountId":"acc"},
               "other":{"type":"oauth","access":"a","refresh":"r","expires":1}}"#,
            0o600,
        );
        let k = r.resolve("typesafe").unwrap().unwrap();
        assert_eq!(k.key.expose(), "ts-file");
        assert_eq!(k.source, KeySource::File);
        let cf = r.resolve("workers-ai").unwrap().unwrap();
        assert_eq!(cf.account_id.as_deref(), Some("acc"));
        assert_eq!(
            r.resolve("other").unwrap(),
            None,
            "oauth entries are not api keys"
        );
    }

    #[test]
    fn env_overrides_file() {
        let dir = tempfile::tempdir().unwrap();
        let r = resolver(
            dir.path(),
            &[
                ("TYPESAFE_API_KEY", "ts-env"),
                ("CLOUDFLARE_API_TOKEN", "cf-env"),
            ],
        );
        write_auth(
            r.auth_path(),
            r#"{"typesafe":{"type":"api_key","key":"ts-file"},
               "workers-ai":{"type":"api_key","key":"cf-file","accountId":"acc-file"}}"#,
            0o600,
        );
        let k = r.resolve("typesafe").unwrap().unwrap();
        assert_eq!(k.key.expose(), "ts-env");
        assert_eq!(k.source, KeySource::Env);
        let cf = r.resolve("workers-ai").unwrap().unwrap();
        assert_eq!(cf.key.expose(), "cf-env");
        assert_eq!(cf.account_id.as_deref(), Some("acc-file"));
    }

    #[test]
    fn env_account_id_overrides_file_account_id() {
        let dir = tempfile::tempdir().unwrap();
        let r = resolver(dir.path(), &[("CLOUDFLARE_ACCOUNT_ID", "acc-env")]);
        write_auth(
            r.auth_path(),
            r#"{"workers-ai":{"type":"api_key","key":"cf-file","accountId":"acc-file"}}"#,
            0o600,
        );
        let cf = r.resolve("workers-ai").unwrap().unwrap();
        assert_eq!(cf.account_id.as_deref(), Some("acc-env"));
    }

    #[test]
    fn refuses_group_or_world_readable_file() {
        for mode in [0o640, 0o604, 0o644, 0o660] {
            let dir = tempfile::tempdir().unwrap();
            let r = resolver(dir.path(), &[]);
            write_auth(
                r.auth_path(),
                r#"{"typesafe":{"type":"api_key","key":"k"}}"#,
                mode,
            );
            let err = r.resolve("typesafe").unwrap_err();
            assert!(matches!(err, SecretsError::InsecureMode { .. }), "{mode:o}");
            assert!(!err.to_string().contains("\"k\""));
        }
    }

    #[test]
    fn writes_are_atomic_private_and_keep_other_entries() {
        let dir = tempfile::tempdir().unwrap();
        let r = resolver(dir.path(), &[]);
        write_auth(
            r.auth_path(),
            r#"{"other":{"type":"oauth","access":"a","refresh":"r","expires":1}}"#,
            0o600,
        );
        r.update(|f| f.set_api_key("typesafe", &Secret::new("new"), None))
            .unwrap();
        let mode = fs::metadata(r.auth_path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let dmode = fs::metadata(r.auth_path().parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dmode, 0o700);
        let v: Value = serde_json::from_str(&fs::read_to_string(r.auth_path()).unwrap()).unwrap();
        assert_eq!(v["other"]["refresh"], "r");
        assert_eq!(v["typesafe"]["key"], "new");
        assert_eq!(v["typesafe"]["type"], "api_key");
        // No temp files left behind.
        let leftovers: Vec<_> = fs::read_dir(r.auth_path().parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn local_token_is_made_once() {
        let dir = tempfile::tempdir().unwrap();
        let r = resolver(dir.path(), &[]);
        let a = r.ensure_local_token().unwrap();
        let b = r.ensure_local_token().unwrap();
        assert_eq!(a, b);
        assert!(a.expose().len() > 32);
    }

    #[test]
    fn list_shows_sources_not_values() {
        let dir = tempfile::tempdir().unwrap();
        let r = resolver(dir.path(), &[("OPENAI_API_KEY", "sk-secret")]);
        r.update(|f| f.set_api_key("typesafe", &Secret::new("ts-secret"), None))
            .unwrap();
        let list = r.list().unwrap();
        let text = format!("{list:?}");
        assert!(!text.contains("secret"));
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].provider, "openai");
        assert_eq!(list[0].source, KeySource::Env);
        assert_eq!(list[1].provider, "typesafe");
        assert_eq!(list[1].source, KeySource::File);
    }

    #[test]
    fn refuses_auth_dir_that_others_can_access() {
        for dmode in [0o770, 0o755, 0o707, 0o711] {
            let dir = tempfile::tempdir().unwrap();
            let r = resolver(dir.path(), &[]);
            write_auth(
                r.auth_path(),
                r#"{"typesafe":{"type":"api_key","key":"k"}}"#,
                0o600,
            );
            let parent = r.auth_path().parent().unwrap();
            fs::set_permissions(parent, fs::Permissions::from_mode(dmode)).unwrap();
            let err = r.resolve("typesafe").unwrap_err();
            assert!(
                matches!(err, SecretsError::Insecure { .. }),
                "{dmode:o}: {err}"
            );
            assert!(r.ensure_local_token().is_err(), "{dmode:o}");
        }
    }

    #[test]
    fn refuses_symlinked_or_non_regular_auth_file() {
        let dir = tempfile::tempdir().unwrap();
        let r = resolver(dir.path(), &[]);
        let target = dir.path().join("elsewhere.json");
        fs::write(&target, r#"{"typesafe":{"type":"api_key","key":"k"}}"#).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        fs::create_dir_all(r.auth_path().parent().unwrap()).unwrap();
        fs::set_permissions(
            r.auth_path().parent().unwrap(),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        std::os::unix::fs::symlink(&target, r.auth_path()).unwrap();
        assert!(r.resolve("typesafe").is_err(), "symlink is refused");

        fs::remove_file(r.auth_path()).unwrap();
        fs::create_dir(r.auth_path()).unwrap();
        fs::set_permissions(r.auth_path(), fs::Permissions::from_mode(0o700)).unwrap();
        let err = r.resolve("typesafe").unwrap_err();
        assert!(matches!(err, SecretsError::Insecure { .. }), "{err}");
    }

    #[test]
    fn refuses_fifo_auth_file_without_blocking() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let r = resolver(dir.path(), &[]);
        let parent = r.auth_path().parent().unwrap();
        fs::create_dir_all(parent).unwrap();
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).unwrap();
        let c = std::ffi::CString::new(r.auth_path().as_os_str().as_bytes()).unwrap();
        // SAFETY: mkfifo(3) with a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        // No writer ever opens the FIFO: a blocking open or read would hang.
        let err = r.resolve("typesafe").unwrap_err();
        assert!(
            matches!(&err, SecretsError::Insecure { reason, .. } if reason == "not a regular file"),
            "{err}"
        );
    }

    #[test]
    fn auth_file_debug_is_redacted() {
        let mut f = AuthFile::default();
        f.set_api_key("typesafe", &Secret::new("hunter2"), Some("acc"));
        let text = format!("{f:?}");
        assert!(!text.contains("hunter2"), "{text}");
        assert!(text.contains("typesafe"), "{text}");
    }

    #[test]
    fn secret_debug_is_redacted() {
        let s = Secret::new("hunter2");
        assert!(!format!("{s:?} {s}").contains("hunter2"));
        let k = ApiKey {
            key: s,
            account_id: None,
            source: KeySource::Env,
        };
        assert!(!format!("{k:?}").contains("hunter2"));
    }
}

#[cfg(test)]
mod lock_tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;
    use std::time::Duration;

    fn mkfifo(path: &Path) {
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: mkfifo(3) with a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    }

    /// Run `f` on a thread; fail if it does not finish before the deadline.
    fn within_deadline<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(Duration::from_secs(5))
            .expect("call hung on a FIFO lock path")
    }

    fn fifo_lock(with_reader: bool) {
        let dir = tempfile::tempdir().unwrap();
        let auth = dir.path().join("s1/auth.json");
        fs::create_dir_all(auth.parent().unwrap()).unwrap();
        fs::set_permissions(auth.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
        let lock_path = auth.with_extension("json.lock");
        mkfifo(&lock_path);
        // A reader makes a non-blocking write open succeed: the fstat check
        // must still refuse the FIFO.
        let _reader = with_reader.then(|| {
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&lock_path)
                .unwrap()
        });
        let r = KeyResolver::new(auth, |_| None);
        let res = within_deadline(move || {
            r.update(|f| f.set_api_key("typesafe", &Secret::new("k"), None))
        });
        assert!(res.is_err(), "FIFO lock file is refused");
    }

    #[test]
    fn fifo_auth_lock_without_reader_is_an_error_not_a_hang() {
        fifo_lock(false);
    }

    #[test]
    fn fifo_auth_lock_with_reader_is_refused() {
        fifo_lock(true);
    }
}
