use std::{
    fs,
    io::{self, Write},
    os::windows::fs::MetadataExt,
    path::Path,
    ptr::{null, null_mut},
    slice,
};
use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    },
    Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT,
};
use zeroize::{Zeroize, Zeroizing};

pub(super) const BACKEND_NAME: &str = "dpapi";
const ENTROPY: &[u8] = b"agent-command-vault-v1";

struct ProtectedBlob {
    blob: CRYPT_INTEGER_BLOB,
    zero_on_drop: bool,
}

impl ProtectedBlob {
    fn empty(zero_on_drop: bool) -> Self {
        Self {
            blob: CRYPT_INTEGER_BLOB {
                cbData: 0,
                pbData: null_mut(),
            },
            zero_on_drop,
        }
    }

    fn as_slice(&self) -> &[u8] {
        if self.blob.pbData.is_null() || self.blob.cbData == 0 {
            &[]
        } else {
            unsafe { slice::from_raw_parts(self.blob.pbData, self.blob.cbData as usize) }
        }
    }
}

impl Drop for ProtectedBlob {
    fn drop(&mut self) {
        if !self.blob.pbData.is_null() {
            if self.zero_on_drop && self.blob.cbData > 0 {
                let bytes = unsafe {
                    slice::from_raw_parts_mut(self.blob.pbData, self.blob.cbData as usize)
                };
                bytes.zeroize();
            }
            unsafe {
                LocalFree(self.blob.pbData.cast());
            }
            self.blob.pbData = null_mut();
            self.blob.cbData = 0;
        }
    }
}

fn to_blob(bytes: &[u8]) -> io::Result<CRYPT_INTEGER_BLOB> {
    let len = u32::try_from(bytes.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Vault payload too large"))?;
    Ok(CRYPT_INTEGER_BLOB {
        cbData: len,
        pbData: bytes.as_ptr().cast_mut(),
    })
}

pub(super) fn protect(_dir: &Path, plaintext: &[u8]) -> io::Result<Vec<u8>> {
    let input = to_blob(plaintext)?;
    let entropy = to_blob(ENTROPY)?;
    let mut output = ProtectedBlob::empty(false);
    let ok = unsafe {
        CryptProtectData(
            &input,
            null(),
            &entropy,
            null_mut(),
            null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output.blob,
        )
    };
    if ok == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Windows DPAPI encryption failed for vault payload",
        ));
    }
    Ok(output.as_slice().to_vec())
}

pub(super) fn unprotect(_dir: &Path, ciphertext: &[u8]) -> io::Result<Zeroizing<Vec<u8>>> {
    if ciphertext.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Vault ciphertext is empty",
        ));
    }
    let input = to_blob(ciphertext)?;
    let entropy = to_blob(ENTROPY)?;
    let mut output = ProtectedBlob::empty(true);
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            null_mut(),
            &entropy,
            null_mut(),
            null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output.blob,
        )
    };
    if ok == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Vault decryption failed: corrupt, tampered, or wrong Windows user",
        ));
    }
    Ok(Zeroizing::new(output.as_slice().to_vec()))
}

pub(super) fn verify_private_file(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("Refused unsafe vault file at {}", path.display()),
        ));
    }
    Ok(())
}

pub(super) fn write_private_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temporary = path.with_extension(format!("{}.next", uuid::Uuid::new_v4()));
    let write_result = (|| -> io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
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
    Ok(())
}
