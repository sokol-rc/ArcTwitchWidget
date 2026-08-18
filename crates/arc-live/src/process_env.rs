/// Result of looking a variable up in another process.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvironmentProbe {
    /// How many matching processes were found.
    pub processes: usize,
    /// Whether at least one of them let us read its environment.
    pub readable: bool,
    /// The value, when it is set.
    pub value: Option<String>,
}

#[cfg(windows)]
mod windows {
    use std::ffi::c_void;
    use std::mem::{size_of, zeroed};

    use anyhow::{Context, Result, anyhow};

    const MAX_PATH: usize = 260;
    const INVALID_HANDLE_VALUE: isize = -1;
    const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const PROCESS_VM_READ: u32 = 0x0010;
    const PEB_PROCESS_PARAMETERS_OFFSET_X64: usize = 0x20;
    const PROCESS_PARAMETERS_ENVIRONMENT_OFFSET_X64: usize = 0x80;
    const MAX_ENVIRONMENT_BYTES: usize = 512 * 1024;

    pub fn environment_values(process_name: &str, variable: &str) -> Vec<String> {
        process_ids(process_name)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|pid| environment_value(pid, variable).ok().flatten())
            .collect()
    }

    /// Same lookup, but keeps apart "the variable is not set" and "the process
    /// would not let us read it". Anti-cheat protected games routinely refuse
    /// the read, and reporting that as a missing variable would raise a false
    /// alarm for people whose capture works fine.
    pub fn environment_probe(process_name: &str, variable: &str) -> super::EnvironmentProbe {
        let mut probe = super::EnvironmentProbe::default();
        for pid in process_ids(process_name).unwrap_or_default() {
            probe.processes += 1;
            match environment_value(pid, variable) {
                Ok(Some(value)) => {
                    probe.readable = true;
                    if !value.trim().is_empty() {
                        probe.value = Some(value);
                        return probe;
                    }
                }
                Ok(None) => probe.readable = true,
                Err(_) => {}
            }
        }
        probe
    }

    fn process_ids(process_name: &str) -> Result<Vec<u32>> {
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(last_error("creating process snapshot"));
        }
        let snapshot = Handle(snapshot);
        let mut entry: ProcessEntry32W = unsafe { zeroed() };
        entry.size = size_of::<ProcessEntry32W>() as u32;
        let mut result = Vec::new();
        let mut ok = unsafe { Process32FirstW(snapshot.0, &mut entry) };
        while ok != 0 {
            let end = entry
                .exe_file
                .iter()
                .position(|value| *value == 0)
                .unwrap_or(entry.exe_file.len());
            let name = String::from_utf16_lossy(&entry.exe_file[..end]);
            if name.eq_ignore_ascii_case(process_name) {
                result.push(entry.process_id);
            }
            ok = unsafe { Process32NextW(snapshot.0, &mut entry) };
        }
        Ok(result)
    }

    fn environment_value(pid: u32, variable: &str) -> Result<Option<String>> {
        let handle =
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, 0, pid) };
        if handle == 0 {
            return Err(last_error("opening launcher process"));
        }
        let handle = Handle(handle);
        let mut wow64 = 0i32;
        if unsafe { IsWow64Process(handle.0, &mut wow64) } == 0 || wow64 != 0 {
            // A 32-bit launcher (EpicGamesLauncher.exe) has a WOW64 PEB whose
            // layout differs from the x64 offsets below, so its environment
            // cannot be read this way. This is "unreadable", NOT "the variable is
            // absent": returning Ok(None) here made the probe report the key log
            // as missing for Epic even when it was set. Surface it as an error so
            // the caller stays silent and installs the variable via setx instead.
            return Err(anyhow!(
                "cannot read a 32-bit launcher's environment from a 64-bit process"
            ));
        }

        let mut info: ProcessBasicInformation = unsafe { zeroed() };
        let status = unsafe {
            NtQueryInformationProcess(
                handle.0,
                0,
                (&mut info as *mut ProcessBasicInformation).cast(),
                size_of::<ProcessBasicInformation>() as u32,
                std::ptr::null_mut(),
            )
        };
        if status < 0 || info.peb == 0 {
            return Err(anyhow!("reading launcher process information failed"));
        }
        let parameters = read_usize(handle.0, info.peb + PEB_PROCESS_PARAMETERS_OFFSET_X64)
            .context("reading launcher process parameters")?;
        let environment = read_usize(
            handle.0,
            parameters + PROCESS_PARAMETERS_ENVIRONMENT_OFFSET_X64,
        )
        .context("reading launcher environment pointer")?;
        if environment == 0 {
            return Ok(None);
        }

        let bytes = read_environment(handle.0, environment)?;
        let wide = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        for entry in wide.split(|value| *value == 0) {
            if entry.is_empty() {
                break;
            }
            let entry = String::from_utf16_lossy(entry);
            if let Some((name, value)) = entry.split_once('=')
                && name.eq_ignore_ascii_case(variable)
            {
                return Ok(Some(value.to_owned()));
            }
        }
        Ok(None)
    }

    fn read_usize(handle: isize, address: usize) -> Result<usize> {
        let mut value = 0usize;
        let buffer = unsafe {
            std::slice::from_raw_parts_mut(
                (&mut value as *mut usize).cast::<u8>(),
                size_of::<usize>(),
            )
        };
        let read = read_partial(handle, address, buffer)?;
        if read != buffer.len() {
            return Err(anyhow!("launcher memory read was incomplete"));
        }
        Ok(value)
    }

    fn read_environment(handle: isize, address: usize) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        while output.len() < MAX_ENVIRONMENT_BYTES {
            let mut chunk = vec![0u8; 4096];
            let read = read_partial(handle, address + output.len(), &mut chunk)?;
            if read == 0 {
                break;
            }
            chunk.truncate(read);
            output.extend_from_slice(&chunk);
            if output
                .windows(4)
                .step_by(2)
                .any(|window| window == [0, 0, 0, 0])
            {
                break;
            }
            if read < 4096 {
                break;
            }
        }
        Ok(output)
    }

    fn read_partial(handle: isize, address: usize, buffer: &mut [u8]) -> Result<usize> {
        let mut read = 0usize;
        let ok = unsafe {
            ReadProcessMemory(
                handle,
                address as *const c_void,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut read,
            )
        };
        if ok == 0 {
            return Err(last_error("reading launcher environment"));
        }
        Ok(read)
    }

    fn last_error(action: &str) -> anyhow::Error {
        anyhow!("{action} failed with Windows error {}", unsafe {
            GetLastError()
        })
    }

    struct Handle(isize);

    impl Drop for Handle {
        fn drop(&mut self) {
            if self.0 != 0 && self.0 != INVALID_HANDLE_VALUE {
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    #[repr(C)]
    struct ProcessBasicInformation {
        exit_status: i32,
        peb: usize,
        affinity_mask: usize,
        base_priority: i32,
        unique_process_id: usize,
        parent_process_id: usize,
    }

    #[repr(C)]
    struct ProcessEntry32W {
        size: u32,
        usage: u32,
        process_id: u32,
        default_heap_id: usize,
        module_id: u32,
        threads: u32,
        parent_process_id: u32,
        base_priority: i32,
        flags: u32,
        exe_file: [u16; MAX_PATH],
    }

    #[link(name = "Kernel32")]
    unsafe extern "system" {
        fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> isize;
        fn Process32FirstW(snapshot: isize, entry: *mut ProcessEntry32W) -> i32;
        fn Process32NextW(snapshot: isize, entry: *mut ProcessEntry32W) -> i32;
        fn OpenProcess(access: u32, inherit: i32, process_id: u32) -> isize;
        fn IsWow64Process(process: isize, wow64: *mut i32) -> i32;
        fn ReadProcessMemory(
            process: isize,
            address: *const c_void,
            buffer: *mut c_void,
            size: usize,
            read: *mut usize,
        ) -> i32;
        fn CloseHandle(handle: isize) -> i32;
        fn GetLastError() -> u32;
    }

    #[link(name = "Ntdll")]
    unsafe extern "system" {
        fn NtQueryInformationProcess(
            process: isize,
            class: u32,
            information: *mut c_void,
            length: u32,
            return_length: *mut u32,
        ) -> i32;
    }
}

#[cfg(windows)]
pub use windows::{environment_probe, environment_values};

#[cfg(not(windows))]
pub fn environment_values(_process_name: &str, _variable: &str) -> Vec<String> {
    Vec::new()
}

#[cfg(not(windows))]
pub fn environment_probe(_process_name: &str, _variable: &str) -> EnvironmentProbe {
    EnvironmentProbe::default()
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn reads_environment_from_current_process() {
        let executable = std::env::current_exe().unwrap();
        let name = executable.file_name().unwrap().to_string_lossy();
        let values = super::environment_values(&name, "PATH");
        assert!(values.iter().any(|value| !value.is_empty()));
    }
}
