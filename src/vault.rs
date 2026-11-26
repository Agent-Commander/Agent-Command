#[cfg(unix)]
#[path = "vault/unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "vault/windows.rs"]
mod platform;

use crate::{auth, soul};
#[cfg(any(unix, test))]
use chacha20poly1305::{
    aead::{Aead, AeadCore, KeyInit, OsRng, Payload},
    ChaCha20Poly1305, Nonce,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fmt, fs,
    io::{self, Read},
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock, PoisonError},
};
use zeroize::Zeroizing;

pub const VAULT_DIRECTORY: &str = "agent-command-vault-v1";
pub const VAULT_FILE: &str = "vault.json";
pub const VAULT_SCHEMA: &str = "agent-command-vault-v1";
pub const DEFAULT_NAMESPACE: &str = "global";
pub const MAX_VAULT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_SECRET_NAME_LEN: usize = 64;
pub const MAX_NAMESPACE_LEN: usize = 64;
pub const MAX_DESCRIPTION_LEN: usize = 256;
pub const MAX_SECRET_VALUE_BYTES: usize = 65_536;

#[cfg(any(unix, test))]
const AEAD_MAGIC: &[u8; 4] = b"acv1";
#[cfg(any(unix, test))]
const AEAD_NONCE_BYTES: usize = 12;
#[cfg(any(unix, test))]
const AEAD_AAD: &[u8] = b"agent-command-vault-v1";
const MIN_REDACTED_LEN: usize = 4;
const REDACTED_MARKER: &str = "[REDACTED]";
const IGNORED_REDACTION_LITERALS: [&str; 4] = ["true", "false", "null", "none"];

static RUNTIME_REDACTOR: OnceLock<Mutex<Vec<Zeroizing<String>>>> = OnceLock::new();

fn redactor_registry() -> &'static Mutex<Vec<Zeroizing<String>>> {
    RUNTIME_REDACTOR.get_or_init(|| Mutex::new(Vec::new()))
}

pub fn register_redacted_value(value: &str) {
    let trimmed = value.trim();
    if trimmed.len() < MIN_REDACTED_LEN
        || IGNORED_REDACTION_LITERALS
            .iter()
            .any(|literal| trimmed.eq_ignore_ascii_case(literal))
    {
        return;
    }
    let mut guard = redactor_registry()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if !guard.iter().any(|existing| existing.as_str() == trimmed) {
        guard.push(Zeroizing::new(trimmed.to_owned()));
        guard.sort_by_key(|entry| std::cmp::Reverse(entry.len()));
    }
}

pub fn redact_values(text: &str) -> String {
    let mut scrubbed = text.to_owned();
    let guard = redactor_registry()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    for secret in guard.iter() {
        if !secret.is_empty() && scrubbed.contains(secret.as_str()) {
            scrubbed = scrubbed.replace(secret.as_str(), REDACTED_MARKER);
        }
    }
    drop(guard);
    soul::redact_secrets(&scrubbed)
}

pub fn redact_json(value: &mut Value) {
    match value {
        Value::String(text) => {
            *text = redact_values(text);
        }
        Value::Array(items) => {
            for item in items {
                redact_json(item);
            }
        }
        Value::Object(map) => {
            for entry in map.values_mut() {
                redact_json(entry);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

pub fn validate_secret_name(name: &str) -> Result<String, String> {
    let trimmed = name.trim();
    let bytes = trimmed.as_bytes();
    let valid = !bytes.is_empty()
        && bytes.len() <= MAX_SECRET_NAME_LEN
        && bytes[0].is_ascii_uppercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'_');
    if !valid {
        return Err(format!(
            "Secret name must match ^[A-Z][A-Z0-9_]{{0,{}}}$",
            MAX_SECRET_NAME_LEN - 1
        ));
    }
    Ok(trimmed.to_owned())
}

pub fn validate_namespace(namespace: &str) -> Result<String, String> {
    let trimmed = namespace.trim();
    let valid = !trimmed.is_empty()
        && trimmed.len() <= MAX_NAMESPACE_LEN
        && !trimmed.contains("..")
        && trimmed
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':' | b'/'));
    if !valid {
        return Err("Namespace must be 1-64 characters of A-Za-z0-9._-:/ without '..'.".into());
    }
    Ok(trimmed.to_owned())
}

pub fn validate_secret_value(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("Secret value must not be empty.".into());
    }
    if value.len() > MAX_SECRET_VALUE_BYTES {
        return Err(format!(
            "Secret value exceeds maximum size of {MAX_SECRET_VALUE_BYTES} bytes."
        ));
    }
    if value.contains('\0') {
        return Err("Secret value must not contain NUL bytes.".into());
    }
    Ok(())
}

pub fn validate_description(description: &str) -> Result<String, String> {
    let trimmed = description.trim();
    if trimmed.contains('\0') {
        return Err("Description must not contain NUL bytes.".into());
    }
    if trimmed.chars().count() > MAX_DESCRIPTION_LEN {
        return Err(format!(
            "Description must be at most {MAX_DESCRIPTION_LEN} characters."
        ));
    }
    if soul::contains_secret_like(trimmed) {
        return Err("Description must not contain secret-like material.".into());
    }
    Ok(trimmed.to_owned())
}

pub fn sha256_prefix8(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    encode_hex(&digest[..4])
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VaultMeta {
    pub name: String,
    pub namespace: String,
    pub description: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub value_bytes: usize,
    pub sha256_8: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VaultCheckResult {
    pub present: Vec<String>,
    pub missing: Vec<String>,
}

mod zeroizing_string {
    use serde::{Deserialize, Deserializer, Serializer};
    use zeroize::Zeroizing;

    pub(super) fn serialize<S>(value: &Zeroizing<String>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(value.as_str())
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<Zeroizing<String>, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Zeroizing::new)
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
struct StoredEntry {
    name: String,
    namespace: String,
    description: String,
    created_at: i64,
    updated_at: i64,
    value_bytes: usize,
    sha256_8: String,
    #[serde(with = "zeroizing_string")]
    value: Zeroizing<String>,
}

impl fmt::Debug for StoredEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StoredEntry")
            .field("name", &self.name)
            .field("namespace", &self.namespace)
            .field("description", &self.description)
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .field("value_bytes", &self.value_bytes)
            .field("sha256_8", &self.sha256_8)
            .field("value", &REDACTED_MARKER)
            .finish()
    }
}

impl StoredEntry {
    fn meta(&self) -> VaultMeta {
        VaultMeta {
            name: self.name.clone(),
            namespace: self.namespace.clone(),
            description: self.description.clone(),
            created_at: self.created_at,
            updated_at: self.updated_at,
            value_bytes: self.value_bytes,
            sha256_8: self.sha256_8.clone(),
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VaultPayload {
    entries: Vec<StoredEntry>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VaultEnvelope {
    schema: String,
    backend: String,
    ciphertext_hex: String,
}

#[derive(Clone)]
pub struct VaultStore {
    profile_root: PathBuf,
    lock: Arc<Mutex<()>>,
}

impl fmt::Debug for VaultStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VaultStore")
            .field("profile_root", &self.profile_root)
            .field("backend", &platform::BACKEND_NAME)
            .finish()
    }
}

impl VaultStore {
    pub fn open(profile_root: PathBuf) -> Self {
        Self {
            profile_root,
            lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn from_env() -> io::Result<Self> {
        Ok(Self::open(auth::root()?))
    }

    pub fn backend_name() -> &'static str {
        platform::BACKEND_NAME
    }

    pub fn vault_path(&self) -> PathBuf {
        self.profile_root.join(VAULT_DIRECTORY).join(VAULT_FILE)
    }

    pub fn list(&self, namespace: Option<&str>) -> io::Result<Vec<VaultMeta>> {
        let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let filter_ns = match namespace {
            Some(ns) => Some(
                validate_namespace(ns)
                    .map_err(|msg| io::Error::new(io::ErrorKind::InvalidInput, msg))?,
            ),
            None => None,
        };
        let payload = self.load_locked()?;
        let mut metas: Vec<VaultMeta> = payload
            .entries
            .iter()
            .filter(|entry| {
                filter_ns
                    .as_deref()
                    .is_none_or(|wanted| entry.namespace == wanted)
            })
            .map(StoredEntry::meta)
            .collect();
        metas.sort_by(|left, right| {
            left.namespace
                .cmp(&right.namespace)
                .then_with(|| left.name.cmp(&right.name))
        });
        Ok(metas)
    }

    pub fn count(&self) -> io::Result<usize> {
        let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(self.load_locked()?.entries.len())
    }

    pub fn check(&self, names: &[String], namespace: Option<&str>) -> io::Result<VaultCheckResult> {
        let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let ns = match namespace {
            Some(ns) => validate_namespace(ns)
                .map_err(|msg| io::Error::new(io::ErrorKind::InvalidInput, msg))?,
            None => DEFAULT_NAMESPACE.to_owned(),
        };
        let mut validated = Vec::with_capacity(names.len());
        let mut seen = BTreeSet::new();
        for raw in names {
            let name = validate_secret_name(raw)
                .map_err(|msg| io::Error::new(io::ErrorKind::InvalidInput, msg))?;
            if seen.insert(name.clone()) {
                validated.push(name);
            }
        }
        let payload = self.load_locked()?;
        let mut present = Vec::new();
        let mut missing = Vec::new();
        for name in validated {
            let found = payload
                .entries
                .iter()
                .any(|entry| entry.name == name && entry.namespace == ns);
            if found {
                present.push(name);
            } else {
                missing.push(name);
            }
        }
        Ok(VaultCheckResult { present, missing })
    }

    pub fn set(
        &self,
        name: &str,
        value: Zeroizing<String>,
        namespace: Option<&str>,
        description: Option<&str>,
    ) -> io::Result<VaultMeta> {
        let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let name = validate_secret_name(name)
            .map_err(|msg| io::Error::new(io::ErrorKind::InvalidInput, msg))?;
        let namespace = match namespace {
            Some(ns) => validate_namespace(ns)
                .map_err(|msg| io::Error::new(io::ErrorKind::InvalidInput, msg))?,
            None => DEFAULT_NAMESPACE.to_owned(),
        };
        validate_secret_value(&value)
            .map_err(|msg| io::Error::new(io::ErrorKind::InvalidInput, msg))?;
        let description = match description {
            Some(desc) if !desc.trim().is_empty() => validate_description(desc)
                .map_err(|msg| io::Error::new(io::ErrorKind::InvalidInput, msg))?,
            _ => String::new(),
        };

        register_redacted_value(&value);
        let mut payload = self.load_locked()?;
        let now = chrono::Utc::now().timestamp();
        let value_bytes = value.len();
        let sha256_8 = sha256_prefix8(&value);

        let meta = if let Some(existing) = payload
            .entries
            .iter_mut()
            .find(|entry| entry.name == name && entry.namespace == namespace)
        {
            existing.value = value;
            existing.value_bytes = value_bytes;
            existing.sha256_8 = sha256_8;
            existing.updated_at = now;
            if !description.is_empty() {
                existing.description = description;
            }
            existing.meta()
        } else {
            let entry = StoredEntry {
                name,
                namespace,
                description,
                created_at: now,
                updated_at: now,
                value_bytes,
                sha256_8,
                value,
            };
            let meta = entry.meta();
            payload.entries.push(entry);
            meta
        };

        self.save_locked(&payload)?;
        Ok(meta)
    }

    pub fn delete(&self, name: &str, namespace: Option<&str>) -> io::Result<bool> {
        let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let name = validate_secret_name(name)
            .map_err(|msg| io::Error::new(io::ErrorKind::InvalidInput, msg))?;
        let namespace = match namespace {
            Some(ns) => validate_namespace(ns)
                .map_err(|msg| io::Error::new(io::ErrorKind::InvalidInput, msg))?,
            None => DEFAULT_NAMESPACE.to_owned(),
        };
        let mut payload = self.load_locked()?;
        let before = payload.entries.len();
        payload
            .entries
            .retain(|entry| !(entry.name == name && entry.namespace == namespace));
        if payload.entries.len() == before {
            return Ok(false);
        }
        self.save_locked(&payload)?;
        Ok(true)
    }

    pub fn resolve_value(
        &self,
        name: &str,
        namespace: Option<&str>,
    ) -> io::Result<Option<Zeroizing<String>>> {
        let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let name = validate_secret_name(name)
            .map_err(|msg| io::Error::new(io::ErrorKind::InvalidInput, msg))?;
        let namespace = match namespace {
            Some(ns) => validate_namespace(ns)
                .map_err(|msg| io::Error::new(io::ErrorKind::InvalidInput, msg))?,
            None => DEFAULT_NAMESPACE.to_owned(),
        };
        let payload = self.load_locked()?;
        Ok(payload
            .entries
            .into_iter()
            .find(|entry| entry.name == name && entry.namespace == namespace)
            .map(|entry| entry.value))
    }

    pub fn resolve_env(
        &self,
        bindings: &[(String, Option<String>)],
    ) -> io::Result<Vec<(String, Zeroizing<String>)>> {
        let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let payload = self.load_locked()?;
        let mut resolved = Vec::new();
        for (raw_name, raw_ns) in bindings {
            let name = validate_secret_name(raw_name)
                .map_err(|msg| io::Error::new(io::ErrorKind::InvalidInput, msg))?;
            let namespace = match raw_ns.as_deref() {
                Some(ns) => validate_namespace(ns)
                    .map_err(|msg| io::Error::new(io::ErrorKind::InvalidInput, msg))?,
                None => DEFAULT_NAMESPACE.to_owned(),
            };
            if let Some(entry) = payload
                .entries
                .iter()
                .find(|entry| entry.name == name && entry.namespace == namespace)
            {
                register_redacted_value(&entry.value);
                resolved.push((name, entry.value.clone()));
            }
        }
        Ok(resolved)
    }

    fn load_locked(&self) -> io::Result<VaultPayload> {
        let Some(dir) = auth::existing_private_state(&self.profile_root, VAULT_DIRECTORY)? else {
            return Ok(VaultPayload::default());
        };
        let path = dir.join(VAULT_FILE);
        match fs::symlink_metadata(&path) {
            Ok(_) => platform::verify_private_file(&path)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(VaultPayload::default());
            }
            Err(error) => return Err(error),
        }
        let raw = fs::read(&path)?;
        if raw.len() > MAX_VAULT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Vault file exceeds maximum size at {}", path.display()),
            ));
        }
        let envelope: VaultEnvelope = serde_json::from_slice(&raw).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Corrupt vault envelope at {}", path.display()),
            )
        })?;
        if envelope.schema != VAULT_SCHEMA {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Unsupported vault schema {} at {}",
                    envelope.schema,
                    path.display()
                ),
            ));
        }
        if envelope.backend != platform::BACKEND_NAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Vault backend mismatch ({}) at {}",
                    envelope.backend,
                    path.display()
                ),
            ));
        }
        let ciphertext = decode_hex(&envelope.ciphertext_hex).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Corrupt vault ciphertext encoding at {}", path.display()),
            )
        })?;
        let plaintext = platform::unprotect(&dir, &ciphertext).map_err(|error| {
            io::Error::new(error.kind(), format!("{} ({})", error, path.display()))
        })?;
        let payload: VaultPayload = serde_json::from_slice(&plaintext).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Corrupt decrypted vault payload at {}", path.display()),
            )
        })?;
        for entry in &payload.entries {
            register_redacted_value(&entry.value);
        }
        Ok(payload)
    }

    fn save_locked(&self, payload: &VaultPayload) -> io::Result<()> {
        let dir = auth::private_state_directory(&self.profile_root, VAULT_DIRECTORY)?;
        let plaintext = Zeroizing::new(serde_json::to_vec(payload).map_err(io::Error::other)?);
        let ciphertext = platform::protect(&dir, &plaintext)?;
        let envelope = VaultEnvelope {
            schema: VAULT_SCHEMA.to_owned(),
            backend: platform::BACKEND_NAME.to_owned(),
            ciphertext_hex: encode_hex(&ciphertext),
        };
        let bytes = serde_json::to_vec_pretty(&envelope).map_err(io::Error::other)?;
        if bytes.len() > MAX_VAULT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Vault exceeds maximum allowed size.",
            ));
        }
        platform::write_private_atomic(&dir.join(VAULT_FILE), &bytes)
    }
}

#[cfg(any(unix, test))]
pub(crate) fn generate_aead_key() -> Zeroizing<[u8; 32]> {
    let key = ChaCha20Poly1305::generate_key(&mut OsRng);
    let mut raw = Zeroizing::new([0u8; 32]);
    raw.copy_from_slice(&key);
    raw
}

#[cfg(any(unix, test))]
pub(crate) fn aead_seal(key: &[u8; 32], plaintext: &[u8]) -> io::Result<Vec<u8>> {
    let cipher = ChaCha20Poly1305::new(key.into());
    let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext,
                aad: AEAD_AAD,
            },
        )
        .map_err(|_| io::Error::other("AEAD encryption failed"))?;
    let mut output = Vec::with_capacity(AEAD_MAGIC.len() + AEAD_NONCE_BYTES + ciphertext.len());
    output.extend_from_slice(AEAD_MAGIC);
    output.extend_from_slice(&nonce);
    output.extend_from_slice(&ciphertext);
    Ok(output)
}

#[cfg(any(unix, test))]
pub(crate) fn aead_open(key: &[u8; 32], blob: &[u8]) -> io::Result<Zeroizing<Vec<u8>>> {
    let header_len = AEAD_MAGIC.len() + AEAD_NONCE_BYTES;
    if blob.len() <= header_len || &blob[..AEAD_MAGIC.len()] != AEAD_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Invalid vault AEAD header",
        ));
    }
    let nonce = Nonce::from_slice(&blob[AEAD_MAGIC.len()..header_len]);
    let ciphertext = &blob[header_len..];
    let cipher = ChaCha20Poly1305::new(key.into());
    let plaintext = cipher
        .decrypt(
            nonce,
            Payload {
                msg: ciphertext,
                aad: AEAD_AAD,
            },
        )
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Vault AEAD authentication failed: corrupt or tampered payload",
            )
        })?;
    Ok(Zeroizing::new(plaintext))
}

pub(crate) fn encode_hex(bytes: &[u8]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(TABLE[(byte >> 4) as usize] as char);
        out.push(TABLE[(byte & 0x0f) as usize] as char);
    }
    out
}

pub(crate) fn decode_hex(hex: &str) -> io::Result<Vec<u8>> {
    let bytes = hex.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Odd-length hex string",
        ));
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let (chunks, _) = bytes.as_chunks::<2>();
    for &[high, low] in chunks {
        let hi = hex_nibble(high)?;
        let lo = hex_nibble(low)?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

#[cfg(any(unix, test))]
pub(crate) fn decode_key_hex(hex: &str) -> io::Result<Zeroizing<[u8; 32]>> {
    let decoded = Zeroizing::new(decode_hex(hex)?);
    if decoded.len() != 32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Vault key must be exactly 32 bytes",
        ));
    }
    let mut key = Zeroizing::new([0u8; 32]);
    key.copy_from_slice(&decoded);
    Ok(key)
}

fn hex_nibble(byte: u8) -> io::Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Non-hex byte in vault encoding",
        )),
    }
}

const CLI_USAGE: &str = "Usage: agent-command vault list [--namespace NS]\n       agent-command vault check NAME... [--namespace NS]\n       agent-command vault set NAME --stdin [--namespace NS] [--desc TEXT]\n       agent-command vault delete NAME [--namespace NS]\n       agent-command vault reveal NAME --confirm [--namespace NS]\n\nSecrets are never accepted as command-line arguments. Pass values via --stdin.";

pub fn run(args: &[String]) -> io::Result<()> {
    if args.is_empty() || args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("{CLI_USAGE}");
        return Ok(());
    }
    let store = VaultStore::from_env()?;
    let mut words = args.iter();
    let Some(sub) = words.next().map(String::as_str) else {
        println!("{CLI_USAGE}");
        return Ok(());
    };
    let mut namespace: Option<String> = None;
    let mut description: Option<String> = None;
    let mut use_stdin = false;
    let mut confirmed = false;
    let mut positionals = Vec::new();
    while let Some(word) = words.next() {
        match word.as_str() {
            "--namespace" => {
                namespace = Some(
                    words
                        .next()
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "--namespace requires a value",
                            )
                        })?
                        .clone(),
                );
            }
            "--desc" | "--description" => {
                description = Some(
                    words
                        .next()
                        .ok_or_else(|| {
                            io::Error::new(io::ErrorKind::InvalidInput, "--desc requires a value")
                        })?
                        .clone(),
                );
            }
            "--stdin" => use_stdin = true,
            "--confirm" => confirmed = true,
            flag if flag.starts_with("--") => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("Unknown vault flag {flag}.\n{CLI_USAGE}"),
                ));
            }
            value => positionals.push(value),
        }
    }
    match sub {
        "list" => {
            if !positionals.is_empty() {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, CLI_USAGE));
            }
            let items = store.list(namespace.as_deref())?;
            if items.is_empty() {
                println!("No secrets stored.");
            } else {
                for item in items {
                    println!(
                        "{}\t{}\t{}B\t{}\t{}",
                        item.namespace,
                        item.name,
                        item.value_bytes,
                        item.sha256_8,
                        item.description
                    );
                }
            }
            Ok(())
        }
        "check" => {
            if positionals.is_empty() {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, CLI_USAGE));
            }
            let names: Vec<String> = positionals.iter().map(|name| (*name).to_owned()).collect();
            let result = store.check(&names, namespace.as_deref())?;
            println!(
                "present: {}\nmissing: {}",
                result.present.join(", "),
                result.missing.join(", ")
            );
            Ok(())
        }
        "set" => {
            if positionals.len() != 1 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Pass only the secret NAME as an argument and pipe the secret value via --stdin.",
                ));
            }
            if !use_stdin {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Refused: pass --stdin so secret values never appear in process arguments.",
                ));
            }
            let mut raw = Zeroizing::new(String::new());
            io::stdin()
                .take((MAX_SECRET_VALUE_BYTES + 2) as u64)
                .read_to_string(&mut raw)?;
            let trimmed = Zeroizing::new(raw.trim_end_matches(['\r', '\n']).to_owned());
            let meta = store.set(
                positionals[0],
                trimmed,
                namespace.as_deref(),
                description.as_deref(),
            )?;
            println!(
                "Stored {}/{} ({} bytes, sha256:{})",
                meta.namespace, meta.name, meta.value_bytes, meta.sha256_8
            );
            Ok(())
        }
        "delete" => {
            if positionals.len() != 1 {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, CLI_USAGE));
            }
            if store.delete(positionals[0], namespace.as_deref())? {
                println!("Deleted {}", positionals[0]);
            } else {
                println!("{} not found", positionals[0]);
            }
            Ok(())
        }
        "reveal" => {
            if positionals.len() != 1 || !confirmed {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Use `agent-command vault reveal NAME --confirm` to reveal a secret.",
                ));
            }
            match store.resolve_value(positionals[0], namespace.as_deref())? {
                Some(secret) => {
                    println!("{}", secret.as_str());
                    Ok(())
                }
                None => Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("Secret {} not found", positionals[0]),
                )),
            }
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Unknown vault command {other}.\n{CLI_USAGE}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "ac-vault-{name}-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn vault_round_trip_encrypts_at_rest_and_never_stores_plaintext() {
        let scratch = Scratch::new("roundtrip");
        let store = VaultStore::open(scratch.0.join("sub-auth"));
        assert!(store.list(None).unwrap().is_empty());

        let raw_secret = "tok_vault_secret_alpha_99112233";
        let meta = store
            .set(
                "MY_SERVICE_TOKEN",
                Zeroizing::new(raw_secret.to_owned()),
                Some("mcp:github"),
                Some("Personal integration token"),
            )
            .unwrap();
        assert_eq!(meta.name, "MY_SERVICE_TOKEN");
        assert_eq!(meta.namespace, "mcp:github");
        assert_eq!(meta.value_bytes, raw_secret.len());
        assert_eq!(meta.sha256_8, sha256_prefix8(raw_secret));

        let on_disk = fs::read_to_string(store.vault_path()).unwrap();
        assert!(!on_disk.contains(raw_secret));
        assert!(!on_disk.contains("tok_vault_secret"));
        assert!(on_disk.contains(VAULT_SCHEMA));

        let listed = store.list(None).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0], meta);
        let listed_json = serde_json::to_string(&listed).unwrap();
        assert!(!listed_json.contains(raw_secret));

        let check = store
            .check(
                &["MY_SERVICE_TOKEN".into(), "MISSING_KEY".into()],
                Some("mcp:github"),
            )
            .unwrap();
        assert_eq!(check.present, vec!["MY_SERVICE_TOKEN"]);
        assert_eq!(check.missing, vec!["MISSING_KEY"]);

        let resolved = store
            .resolve_value("MY_SERVICE_TOKEN", Some("mcp:github"))
            .unwrap()
            .unwrap();
        assert_eq!(resolved.as_str(), raw_secret);

        let env_pairs = store
            .resolve_env(&[("MY_SERVICE_TOKEN".into(), Some("mcp:github".into()))])
            .unwrap();
        assert_eq!(env_pairs.len(), 1);
        assert_eq!(env_pairs[0].0, "MY_SERVICE_TOKEN");
        assert_eq!(env_pairs[0].1.as_str(), raw_secret);

        assert!(store
            .delete("MY_SERVICE_TOKEN", Some("mcp:github"))
            .unwrap());
        assert!(!store
            .delete("MY_SERVICE_TOKEN", Some("mcp:github"))
            .unwrap());
        assert!(store.list(None).unwrap().is_empty());
    }

    #[test]
    fn corrupt_or_tampered_vault_fails_closed() {
        let scratch = Scratch::new("tamper");
        let store = VaultStore::open(scratch.0.join("sub-auth"));
        store
            .set(
                "API_KEY_ONE",
                Zeroizing::new("secret_value_for_tamper_test".to_owned()),
                None,
                None,
            )
            .unwrap();

        let path = store.vault_path();
        let original = fs::read_to_string(&path).unwrap();
        let mut parsed: Value = serde_json::from_str(&original).unwrap();
        let hex = parsed["ciphertext_hex"].as_str().unwrap().to_owned();
        let flipped_first = if hex.starts_with('0') { 'f' } else { '0' };
        let tampered_hex = format!("{flipped_first}{}", &hex[1..]);
        parsed["ciphertext_hex"] = Value::String(tampered_hex);
        fs::write(&path, serde_json::to_vec(&parsed).unwrap()).unwrap();

        let err = store.list(None).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains(VAULT_FILE));

        fs::write(&path, b"{corrupt json").unwrap();
        assert_eq!(
            store.list(None).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn aead_codec_round_trips_and_rejects_tampering_on_every_os() {
        let key = generate_aead_key();
        let plaintext = b"{\"entries\":[{\"name\":\"DEMO\"}]}";
        let sealed = aead_seal(&key, plaintext).unwrap();
        assert!(sealed.starts_with(AEAD_MAGIC));
        assert!(!sealed.windows(4).any(|w| w == b"DEMO"));
        let opened = aead_open(&key, &sealed).unwrap();
        assert_eq!(opened.as_slice(), plaintext);

        let mut tampered = sealed.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;
        assert_eq!(
            aead_open(&key, &tampered).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        let wrong_key = generate_aead_key();
        assert_eq!(
            aead_open(&wrong_key, &sealed).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        let key_hex = encode_hex(&*key);
        let decoded_key = decode_key_hex(&key_hex).unwrap();
        assert_eq!(&*decoded_key, &*key);
        assert_eq!(
            decode_key_hex("deadbeef").unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn validation_and_runtime_redactor_block_leaks() {
        assert!(validate_secret_name("OPENAI_API_KEY").is_ok());
        assert!(validate_secret_name("lowercase_key").is_err());
        assert!(validate_secret_name("123_KEY").is_err());
        assert!(validate_secret_name("BAD-KEY").is_err());
        assert!(validate_namespace("mcp:linear").is_ok());
        assert!(validate_namespace("../escape").is_err());
        assert!(validate_secret_value("").is_err());
        assert!(validate_secret_value("has\0nul").is_err());
        assert!(validate_description("sk-proj-0123456789abcdefghijkl").is_err());

        let token = "custom_vault_secret_xyz987654";
        register_redacted_value(token);
        let scrubbed = redact_values(&format!("Header used {token} in output"));
        assert!(!scrubbed.contains(token));
        assert!(scrubbed.contains(REDACTED_MARKER));

        let mut payload = serde_json::json!({"nested": [format!("leak={token}")]});
        redact_json(&mut payload);
        assert!(!payload.to_string().contains(token));
    }

    #[test]
    fn stored_entry_debug_never_prints_secret_value() {
        let entry = StoredEntry {
            name: "SECRET_X".into(),
            namespace: DEFAULT_NAMESPACE.into(),
            description: "desc".into(),
            created_at: 1,
            updated_at: 2,
            value_bytes: 18,
            sha256_8: "12345678".into(),
            value: Zeroizing::new("never_print_this_1".into()),
        };
        let rendered = format!("{entry:?}");
        assert!(!rendered.contains("never_print_this_1"));
        assert!(rendered.contains(REDACTED_MARKER));
    }
}
