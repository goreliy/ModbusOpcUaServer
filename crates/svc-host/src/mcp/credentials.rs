use std::path::Path;

/// Protect the credential before writing it. The selected configuration is the
/// access boundary: only principals able to write it may control its endpoint.
#[cfg(windows)]
pub(super) fn protect(path: &Path, config: &Path) -> Result<(), String> {
    windows::copy_config_dacl(path, config)
}

#[cfg(not(windows))]
pub(super) fn protect(path: &Path, _config: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("cannot protect MCP credential {}: {e}", path.display()))
}

#[cfg(windows)]
mod windows {
    use std::{os::windows::ffi::OsStrExt, path::Path, ptr};
    use windows_sys::Win32::{
        Foundation::{LocalFree, ERROR_SUCCESS, GENERIC_ALL, GENERIC_WRITE},
        Security::{
            AddAce,
            Authorization::{GetNamedSecurityInfoW, SetNamedSecurityInfoW, SE_FILE_OBJECT},
            GetAce, InitializeAcl, ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION,
            INHERIT_ONLY_ACE, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
        },
        Storage::FileSystem::{FILE_READ_DATA, FILE_WRITE_DATA},
    };

    fn wide(path: &Path) -> Result<Vec<u16>, String> {
        let value: Vec<u16> = path.as_os_str().encode_wide().collect();
        if value.contains(&0) {
            return Err("MCP credential path contains a NUL character".into());
        }
        Ok(value.into_iter().chain(std::iter::once(0)).collect())
    }

    struct SecurityDescriptor(PSECURITY_DESCRIPTOR);
    impl Drop for SecurityDescriptor {
        fn drop(&mut self) {
            // GetNamedSecurityInfoW allocates this descriptor with LocalAlloc;
            // the DACL is owned by that descriptor and is never freed separately.
            unsafe { LocalFree(self.0) };
        }
    }

    fn writer_dacl(dacl: *const ACL) -> Result<Vec<u32>, String> {
        let revision = unsafe { (*dacl).AclRevision } as u32;
        let mut entries = Vec::<Vec<u8>>::new();
        let mut read_denies = Vec::<Vec<u8>>::new();
        let mut writers = 0;
        for index in 0..unsafe { (*dacl).AceCount } {
            let mut ace = ptr::null_mut();
            if unsafe { GetAce(dacl, index as u32, &mut ace) } == 0 {
                return Err(format!(
                    "cannot read MCP configuration permission entry: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let header = unsafe { &*ace.cast::<ACE_HEADER>() };
            if u32::from(header.AceFlags) & INHERIT_ONLY_ACE != 0 {
                continue;
            }
            if usize::from(header.AceSize) < std::mem::size_of::<ACCESS_ALLOWED_ACE>() {
                return Err("invalid MCP configuration permission entry".into());
            }
            let mask = unsafe { (*ace.cast::<ACCESS_ALLOWED_ACE>()).Mask };
            let writable = mask & (FILE_WRITE_DATA | GENERIC_WRITE | GENERIC_ALL) != 0;
            let keep = match header.AceType {
                // Retain each standard/conditional deny unchanged. A deny of
                // config writing must also prevent reading the control token,
                // even when another group has a write-capable allow entry.
                1 | 10 => {
                    if writable {
                        let mut deny = unsafe {
                            std::slice::from_raw_parts(ace.cast::<u8>(), header.AceSize as usize)
                        }
                        .to_vec();
                        deny[4..8].copy_from_slice(&FILE_READ_DATA.to_le_bytes());
                        read_denies.push(deny);
                    }
                    true
                }
                // Object-specific denies may apply only to a particular
                // property, which cannot be safely mapped to token access.
                6 | 12 => {
                    return Err(
                        "unsupported object-specific MCP configuration deny permission".into(),
                    )
                }
                // Standard, object and conditional allow ACEs all store Mask
                // after ACE_HEADER. Copy their original SIDs and predicates.
                0 | 5 | 9 | 11 => {
                    if writable {
                        writers += 1;
                        true
                    } else {
                        false
                    }
                }
                _ => {
                    return Err(format!(
                        "unsupported MCP configuration permission entry type {}",
                        header.AceType
                    ))
                }
            };
            if keep {
                entries.push(
                    unsafe {
                        std::slice::from_raw_parts(ace.cast::<u8>(), header.AceSize as usize)
                    }
                    .to_vec(),
                );
            }
        }
        if writers == 0 {
            return Err("MCP configuration permissions contain no writable principal".into());
        }
        let size = std::mem::size_of::<ACL>()
            + read_denies
                .iter()
                .chain(&entries)
                .map(Vec::len)
                .sum::<usize>();
        if size > u16::MAX as usize {
            return Err("MCP configuration permissions exceed the Windows ACL size limit".into());
        }
        // u32 storage supplies alignment for ACL. Additional read denies come
        // first, so even an inherited write deny cannot be bypassed by a prior
        // allow when opening the token. Original ACE ordering remains intact.
        let mut storage = vec![0u32; size.div_ceil(std::mem::size_of::<u32>())];
        let filtered = storage.as_mut_ptr().cast::<ACL>();
        if unsafe { InitializeAcl(filtered, size as u32, revision) } == 0 {
            return Err(format!(
                "cannot initialize MCP credential permissions: {}",
                std::io::Error::last_os_error()
            ));
        }
        for entry in read_denies.iter().chain(&entries) {
            if unsafe {
                AddAce(
                    filtered,
                    revision,
                    u32::MAX,
                    entry.as_ptr().cast(),
                    entry.len() as u32,
                )
            } == 0
            {
                return Err(format!(
                    "cannot copy MCP configuration permission entry: {}",
                    std::io::Error::last_os_error()
                ));
            }
        }
        Ok(storage)
    }

    pub(super) fn copy_config_dacl(path: &Path, config: &Path) -> Result<(), String> {
        let config_wide = wide(config)?;
        let path_wide = wide(path)?;
        let mut dacl: *mut ACL = ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // These pointers are valid for this synchronous call and the returned
        // DACL remains valid until descriptor is dropped below.
        let result = unsafe {
            GetNamedSecurityInfoW(
                config_wide.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut dacl,
                ptr::null_mut(),
                &mut descriptor,
            )
        };
        if result != ERROR_SUCCESS {
            return Err(format!(
                "cannot read MCP configuration permissions for {}: {}",
                config.display(),
                std::io::Error::from_raw_os_error(result as i32)
            ));
        }
        let _descriptor = SecurityDescriptor(descriptor);
        // A NULL DACL grants everyone full access. Refuse to expose a bearer
        // token through such a configuration instead of copying that grant.
        if dacl.is_null() {
            return Err(format!(
                "cannot protect MCP credential: configuration {} has no access-control list",
                config.display()
            ));
        }
        // Config read access is not permission to control the server. Retain
        // write-capable grants and every effective deny, and block inheritance
        // so a directory's Users read grant cannot expose the bearer token.
        let filtered = writer_dacl(dacl)?;
        let result = unsafe {
            SetNamedSecurityInfoW(
                path_wide.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                filtered.as_ptr().cast::<ACL>(),
                ptr::null_mut(),
            )
        };
        if result != ERROR_SUCCESS {
            return Err(format!(
                "cannot apply MCP configuration permissions to {}: {}",
                path.display(),
                std::io::Error::from_raw_os_error(result as i32)
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use windows_sys::Win32::Security::{
            Authorization::{
                ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
            },
            GetSecurityDescriptorControl, GetSecurityDescriptorDacl, SE_DACL_PROTECTED,
        };

        fn apply_sddl(path: &Path, sddl: &str) {
            let sddl: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
            let mut descriptor = ptr::null_mut();
            assert_ne!(
                unsafe {
                    ConvertStringSecurityDescriptorToSecurityDescriptorW(
                        sddl.as_ptr(),
                        SDDL_REVISION_1,
                        &mut descriptor,
                        ptr::null_mut(),
                    )
                },
                0
            );
            let descriptor = SecurityDescriptor(descriptor);
            let mut dacl = ptr::null_mut();
            let mut present = 0;
            let mut defaulted = 0;
            assert_ne!(
                unsafe {
                    GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut dacl, &mut defaulted)
                },
                0
            );
            assert_ne!(present, 0);
            assert_eq!(
                unsafe {
                    SetNamedSecurityInfoW(
                        wide(path).unwrap().as_ptr(),
                        SE_FILE_OBJECT,
                        DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                        ptr::null_mut(),
                        ptr::null_mut(),
                        dacl,
                        ptr::null_mut(),
                    )
                },
                ERROR_SUCCESS
            );
        }

        fn read_aces(path: &Path) -> (Vec<Vec<u8>>, u16) {
            let mut descriptor = ptr::null_mut();
            let mut dacl = ptr::null_mut();
            assert_eq!(
                unsafe {
                    GetNamedSecurityInfoW(
                        wide(path).unwrap().as_ptr(),
                        SE_FILE_OBJECT,
                        DACL_SECURITY_INFORMATION,
                        ptr::null_mut(),
                        ptr::null_mut(),
                        &mut dacl,
                        ptr::null_mut(),
                        &mut descriptor,
                    )
                },
                ERROR_SUCCESS
            );
            let descriptor = SecurityDescriptor(descriptor);
            assert!(!dacl.is_null());
            let mut control = 0;
            let mut revision = 0;
            assert_ne!(
                unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) },
                0
            );
            let mut aces = Vec::new();
            for index in 0..unsafe { (*dacl).AceCount } {
                let mut ace = ptr::null_mut();
                assert_ne!(unsafe { GetAce(dacl, index as u32, &mut ace) }, 0);
                let header = unsafe { &*ace.cast::<ACE_HEADER>() };
                aces.push(
                    unsafe {
                        std::slice::from_raw_parts(ace.cast::<u8>(), header.AceSize as usize)
                    }
                    .to_vec(),
                );
            }
            (aces, control)
        }

        #[test]
        fn copies_operator_permissions_without_directory_grants() {
            let dir = tempfile::tempdir().unwrap();
            let config = dir.path().join("config.json");
            let token = dir.path().join("credential.json");
            std::fs::write(&config, "{}").unwrap();
            std::fs::write(&token, "").unwrap();
            // Users may read config; the synthetic operator has Modify. Read
            // grants must not turn into server-control credentials.
            apply_sddl(
                &config,
                "D:P(D;;FW;;;S-1-5-21-111-222-333-5555)(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;OW)(A;;0x1301bf;;;S-1-5-21-111-222-333-4444)(A;;FRFX;;;BU)",
            );
            apply_sddl(&token, "D:P(A;;FA;;;OW)(A;;FR;;;WD)");
            copy_config_dacl(&token, &config).unwrap();
            let (config_aces, _) = read_aces(&config);
            let (token_aces, control) = read_aces(&token);
            assert_eq!(token_aces[1..], config_aces[..config_aces.len() - 1]);
            let mut read_deny = config_aces[0].clone();
            read_deny[4..8].copy_from_slice(&FILE_READ_DATA.to_le_bytes());
            assert_eq!(token_aces[0], read_deny);
            assert_eq!(
                token_aces.len(),
                6,
                "read deny + original deny + SYSTEM/admin/owner/operator remain; Users read disappears"
            );
            assert_ne!(control & SE_DACL_PROTECTED, 0);
            std::fs::write(&token, "protected bearer token").unwrap();
        }

        #[test]
        fn refuses_unrestricted_configuration_and_missing_configuration() {
            let dir = tempfile::tempdir().unwrap();
            let config = dir.path().join("config.json");
            let token = dir.path().join("credential.json");
            std::fs::write(&config, "{}").unwrap();
            std::fs::write(&token, "").unwrap();
            apply_sddl(&config, "D:NO_ACCESS_CONTROL");
            assert!(copy_config_dacl(&token, &config)
                .unwrap_err()
                .contains("no access-control list"));
            assert!(copy_config_dacl(&token, &dir.path().join("absent.json"))
                .unwrap_err()
                .contains("cannot read MCP configuration permissions"));
        }

        #[test]
        fn refuses_configuration_with_only_read_grants() {
            let dir = tempfile::tempdir().unwrap();
            let config = dir.path().join("config.json");
            let token = dir.path().join("credential.json");
            std::fs::write(&config, "{}").unwrap();
            std::fs::write(&token, "").unwrap();
            apply_sddl(&config, "D:P(A;;FR;;;OW)(A;;FR;;;BU)");
            assert!(copy_config_dacl(&token, &config)
                .unwrap_err()
                .contains("no writable principal"));
        }

        #[test]
        fn writer_deny_also_denies_token_read_even_with_full_control_allow() {
            let dir = tempfile::tempdir().unwrap();
            let config = dir.path().join("config.json");
            let token = dir.path().join("credential.json");
            std::fs::write(&config, "readable configuration").unwrap();
            std::fs::write(&token, "secret").unwrap();
            apply_sddl(&config, "D:P(D;;0x2;;;OW)(A;;FA;;;OW)(A;;FA;;;SY)");
            assert!(
                std::fs::read(&config).is_ok(),
                "the denied writer may still read config"
            );
            assert_eq!(
                std::fs::write(&config, "changed").unwrap_err().kind(),
                std::io::ErrorKind::PermissionDenied
            );
            copy_config_dacl(&token, &config).unwrap();
            assert_eq!(
                std::fs::read(&token).unwrap_err().kind(),
                std::io::ErrorKind::PermissionDenied
            );
            // READ_CONTROL/WRITE_DAC are still granted to the owner, allowing
            // cleanup without weakening the access test above.
            apply_sddl(&config, "D:P(A;;FA;;;OW)");
            apply_sddl(&token, "D:P(A;;FA;;;OW)");
        }

        #[test]
        fn rejects_object_specific_denies_without_exposing_token() {
            let dir = tempfile::tempdir().unwrap();
            let config = dir.path().join("config.json");
            let token = dir.path().join("credential.json");
            std::fs::write(&config, "{}").unwrap();
            std::fs::write(&token, "").unwrap();
            apply_sddl(
                &config,
                "D:P(OD;;FW;00112233-4455-6677-8899-aabbccddeeff;;S-1-5-21-111-222-333-5555)(A;;FA;;;OW)",
            );
            assert!(copy_config_dacl(&token, &config)
                .unwrap_err()
                .contains("object-specific"));
        }
    }
}
