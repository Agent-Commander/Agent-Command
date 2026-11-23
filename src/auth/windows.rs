use std::{ffi::c_void, fs, io, os::windows::fs::MetadataExt, path::Path, ptr::null_mut};
use windows_sys::Win32::{
    Foundation::{CloseHandle, LocalFree, HANDLE},
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            GetNamedSecurityInfoW, SE_FILE_OBJECT,
        },
        EqualSid, GetAce, GetSecurityDescriptorControl, GetTokenInformation, TokenUser,
        ACCESS_ALLOWED_ACE, ACL, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION,
        OBJECT_INHERIT_ACE, OWNER_SECURITY_INFORMATION, PSID, SECURITY_ATTRIBUTES,
        SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER,
    },
    Storage::FileSystem::{CreateDirectoryW, FILE_ALL_ACCESS, FILE_ATTRIBUTE_REPARSE_POINT},
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

struct Allocation(*mut c_void);
impl Drop for Allocation {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                LocalFree(self.0);
            }
        }
    }
}

struct Token(HANDLE);
impl Drop for Token {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn check(result: i32) -> io::Result<()> {
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn with_user_sid<T>(use_sid: impl FnOnce(PSID) -> io::Result<T>) -> io::Result<T> {
    let mut token = Token(null_mut());
    check(unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token.0) })?;
    let mut bytes = 0;
    unsafe {
        GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut bytes);
    }
    if bytes == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut storage = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
    check(unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            storage.as_mut_ptr().cast(),
            bytes,
            &mut bytes,
        )
    })?;
    use_sid(unsafe { (*storage.as_ptr().cast::<TOKEN_USER>()).User.Sid })
}

fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;
    let mut wide: Vec<_> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "NUL in profile path",
        ));
    }
    wide.push(0);
    Ok(wide)
}

fn denied(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "Expected a directory with user-only access at {}",
            path.display()
        ),
    )
}

pub(super) fn verify(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(denied(path));
    }
    let path_wide = wide_path(path)?;
    with_user_sid(|user| {
        let mut owner = null_mut();
        let mut acl: *mut ACL = null_mut();
        let mut descriptor = Allocation(null_mut());
        let result = unsafe {
            GetNamedSecurityInfoW(
                path_wide.as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                &mut acl,
                null_mut(),
                &mut descriptor.0,
            )
        };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result as i32));
        }
        let mut control = 0;
        let mut revision = 0;
        check(unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) })?;
        if acl.is_null()
            || owner.is_null()
            || control & SE_DACL_PROTECTED == 0
            || unsafe { EqualSid(user, owner) == 0 || (*acl).AceCount != 1 }
        {
            return Err(denied(path));
        }
        let mut ace = null_mut();
        check(unsafe { GetAce(acl, 0, &mut ace) })?;
        let header = unsafe { &*ace.cast::<windows_sys::Win32::Security::ACE_HEADER>() };
        if header.AceType != 0 || usize::from(header.AceSize) < size_of::<ACCESS_ALLOWED_ACE>() {
            return Err(denied(path));
        }
        let ace = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
        if ace.Header.AceFlags != (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE) as u8
            || ace.Mask != FILE_ALL_ACCESS
            || unsafe { EqualSid(user, (&raw const ace.SidStart).cast_mut().cast()) } == 0
        {
            return Err(denied(path));
        }
        Ok(())
    })
}

pub(super) fn create(path: &Path) -> io::Result<()> {
    let wide = wide_path(path)?;
    let result = with_user_sid(|user| {
        let mut sid = null_mut();
        check(unsafe { ConvertSidToStringSidW(user, &mut sid) })?;
        let _sid = Allocation(sid.cast());
        let length = (0..)
            .take_while(|index| unsafe { *sid.add(*index) } != 0)
            .count();
        let sid_text = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(sid, length) });
        let sddl: Vec<u16> = format!("O:{sid_text}D:P(A;OICI;FA;;;{sid_text})")
            .encode_utf16()
            .chain([0])
            .collect();
        let mut descriptor = Allocation(null_mut());
        check(unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor.0,
                null_mut(),
            )
        })?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        check(unsafe { CreateDirectoryW(wide.as_ptr(), &attributes) })
    });
    if let Err(error) = result {
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error);
        }
    }
    verify(path)
}

#[cfg(test)]
#[test]
fn creates_and_validates_user_only_directory() {
    let parent = std::env::temp_dir().join(format!("agent-command-acl-{}", std::process::id()));
    fs::create_dir(&parent).unwrap();
    let private = parent.join("private");
    assert!(verify(&parent).is_err());
    create(&private).unwrap();
    verify(&private).unwrap();
    create(&private).unwrap();
    fs::remove_dir_all(parent).unwrap();
}
