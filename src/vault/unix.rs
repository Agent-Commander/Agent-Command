use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};
use zeroize::Zeroizing;

pub(super) const BACKEND_NAME: &str = "chacha20poly1305-keyfile";
const KEY_FILE_NAME: &str = "vault.key";

fn sync_parent(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        let directory = File::open(parent)?;
        directory.sync_all()?;
    }
    Ok(())
}

pub(super) fn verify_private_file(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7777 != 0o600
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "Expected an owned mode 0600 vault file at {}",
                path.display()
            ),
        ));
    }
    Ok(())
}

pub(super) fn write_private_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temporary = path.with_extension(format!("{}.next", uuid::Uuid::new_v4()));
    let write_result = (|| -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    sync_parent(path)
}

fn read_key(path: &Path) -> io::Result<Option<Zeroizing<[u8; 32]>>> {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            verify_private_file(path)?;
            let text = Zeroizing::new(fs::read_to_string(path)?);
            let decoded = super::decode_key_hex(text.trim())?;
            Ok(Some(decoded))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn load_or_create_key(dir: &Path) -> io::Result<Zeroizing<[u8; 32]>> {
    let key_path = dir.join(KEY_FILE_NAME);
    if let Some(existing) = read_key(&key_path)? {
        return Ok(existing);
    }
    let generated = super::generate_aead_key();
    let encoded = Zeroizing::new(format!("{}\n", super::encode_hex(&*generated)));
    let temporary = key_path.with_extension(format!("{}.next", uuid::Uuid::new_v4()));
    let create_result = (|| -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(encoded.as_bytes())?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(error) = create_result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    match fs::hard_link(&temporary, &key_path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
    }
    let _ = fs::remove_file(&temporary);
    sync_parent(&key_path)?;
    read_key(&key_path)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Vault key file missing after creation at {}",
                key_path.display()
            ),
        )
    })
}

pub(super) fn protect(dir: &Path, plaintext: &[u8]) -> io::Result<Vec<u8>> {
    let key = load_or_create_key(dir)?;
    super::aead_seal(&key, plaintext)
}

pub(super) fn unprotect(dir: &Path, ciphertext: &[u8]) -> io::Result<Zeroizing<Vec<u8>>> {
    let key_path = dir.join(KEY_FILE_NAME);
    let key = read_key(&key_path)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("Vault key file missing at {}", key_path.display()),
        )
    })?;
    super::aead_open(&key, ciphertext)
}
