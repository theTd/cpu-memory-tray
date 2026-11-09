//! Current-user logon startup. Normal launches only read this setting.
use std::{io, os::windows::ffi::OsStrExt, path::Path, ptr};
use windows_sys::Win32::{
    Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS},
    System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_SZ,
        RegCloseKey, RegCreateKeyExW, RegDeleteKeyValueW, RegGetValueW, RegSetValueExW,
    },
};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "CpuMemoryTray";

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

fn checked(status: u32) -> io::Result<()> {
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(status as i32))
    }
}

struct RegistryKey(HKEY);
impl Drop for RegistryKey {
    fn drop(&mut self) {
        // SAFETY: This wrapper owns a successfully created/opened registry handle.
        unsafe {
            RegCloseKey(self.0);
        }
    }
}

fn quoted_command(exe: &Path) -> io::Result<Vec<u16>> {
    let path: Vec<u16> = exe.as_os_str().encode_wide().collect();
    // Run values support at most 260 command-line characters. Do not silently
    // truncate a path, and quote the whole executable even if it has no spaces.
    if path.is_empty() || path.len() + 2 > 260 || path.contains(&0) || path.contains(&34) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "程序路径无效或过长，请将 EXE 移到较短的固定路径后重试",
        ));
    }
    let mut command = Vec::with_capacity(path.len() + 3);
    command.push(34);
    command.extend(path);
    command.extend([34, 0]);
    Ok(command)
}

fn read_enabled(key: &str, name: &str) -> io::Result<bool> {
    let mut data = [0u16; 261];
    let mut size = std::mem::size_of_val(&data) as u32;
    // SAFETY: NUL-terminated names and a correctly sized writable UTF-16 buffer.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            wide(key).as_ptr(),
            wide(name).as_ptr(),
            RRF_RT_REG_SZ,
            ptr::null_mut(),
            data.as_mut_ptr().cast(),
            &mut size,
        )
    };
    match status {
        ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => Ok(false),
        // An oversized existing value should still be possible to remove.
        ERROR_MORE_DATA => Ok(true),
        ERROR_SUCCESS => Ok(data.iter().any(|unit| *unit != 0)),
        _ => Err(io::Error::from_raw_os_error(status as i32)),
    }
}

fn write_enabled(key: &str, name: &str, command: Option<&[u16]>) -> io::Result<()> {
    let key_name = wide(key);
    let value_name = wide(name);
    if let Some(command) = command {
        let mut raw = ptr::null_mut();
        // SAFETY: Output handle is wrapped immediately after successful creation.
        checked(unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                key_name.as_ptr(),
                0,
                ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                ptr::null(),
                &mut raw,
                ptr::null_mut(),
            )
        })?;
        let owned = RegistryKey(raw);
        // SAFETY: Command includes its NUL terminator; byte count matches the slice.
        checked(unsafe {
            RegSetValueExW(
                owned.0,
                value_name.as_ptr(),
                0,
                REG_SZ,
                command.as_ptr().cast(),
                std::mem::size_of_val(command) as u32,
            )
        })
    } else {
        // SAFETY: Delete only this application's named value, never the Run key.
        let status = unsafe {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, key_name.as_ptr(), value_name.as_ptr())
        };
        match status {
            ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => Ok(()),
            _ => checked(status),
        }
    }
}

pub fn enabled() -> io::Result<bool> {
    read_enabled(RUN_KEY, VALUE_NAME)
}

pub fn set_enabled(enabled: bool) -> io::Result<()> {
    let command = if enabled {
        Some(quoted_command(&std::env::current_exe()?)?)
    } else {
        None
    };
    write_enabled(RUN_KEY, VALUE_NAME, command.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executable_paths_are_quoted_and_never_truncated() {
        let command = quoted_command(Path::new(r"C:\程序 目录\监控.exe")).unwrap();
        assert_eq!(
            String::from_utf16(&command[..command.len() - 1]).unwrap(),
            "\"C:\\程序 目录\\监控.exe\""
        );
        assert_eq!(command.last(), Some(&0));
        assert!(quoted_command(Path::new(&"a".repeat(259))).is_err());
        assert!(quoted_command(Path::new(r#"C:\bad"path.exe"#)).is_err());
    }

    #[test]
    fn registry_roundtrip_keeps_other_entries_and_removes_its_own() {
        // Isolated test key: the real login startup settings are never modified.
        let key = format!(r"Software\CpuMemoryTrayTests-{}", std::process::id());
        struct Cleanup(String);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                // SAFETY: The uniquely named test key is owned entirely by this test.
                unsafe {
                    windows_sys::Win32::System::Registry::RegDeleteTreeW(
                        HKEY_CURRENT_USER,
                        wide(&self.0).as_ptr(),
                    );
                }
            }
        }
        let _cleanup = Cleanup(key.clone());
        assert!(!read_enabled(&key, VALUE_NAME).unwrap());
        let command = quoted_command(Path::new(r"C:\测试 目录\monitor.exe")).unwrap();
        write_enabled(&key, "Unrelated", Some(&command)).unwrap();
        for _ in 0..32 {
            write_enabled(&key, VALUE_NAME, Some(&command)).unwrap();
            assert!(read_enabled(&key, VALUE_NAME).unwrap());
            let mut actual = [0u16; 261];
            let mut size = std::mem::size_of_val(&actual) as u32;
            // SAFETY: Test reads back the exact bytes stored under its own test key.
            checked(unsafe {
                RegGetValueW(
                    HKEY_CURRENT_USER,
                    wide(&key).as_ptr(),
                    wide(VALUE_NAME).as_ptr(),
                    RRF_RT_REG_SZ,
                    ptr::null_mut(),
                    actual.as_mut_ptr().cast(),
                    &mut size,
                )
            })
            .unwrap();
            assert_eq!(&actual[..command.len()], command);
            write_enabled(&key, VALUE_NAME, None).unwrap();
            assert!(!read_enabled(&key, VALUE_NAME).unwrap());
        }
        write_enabled(&key, VALUE_NAME, None).unwrap();
        assert!(read_enabled(&key, "Unrelated").unwrap());
    }
}
