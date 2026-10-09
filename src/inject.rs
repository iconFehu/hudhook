//! Facilities for injecting compiled DLLs into target processes.

use std::ffi::{c_void, OsString};
use std::mem::{self, size_of};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use tracing::debug;
#[cfg(target_arch = "x86")]
use windows::core::PCSTR;
#[cfg(target_arch = "x86_64")]
use windows::core::PCWSTR;
use windows::core::{s, w, Error, Result, HRESULT, HSTRING};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_BAD_LENGTH, ERROR_NO_MORE_FILES, HANDLE, WAIT_OBJECT_0,
};
use windows::Win32::System::Diagnostics::Debug::{ReadProcessMemory, WriteProcessMemory};
#[cfg(target_arch = "x86")]
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32First, Process32Next, PROCESSENTRY32, TH32CS_SNAPPROCESS,
};
#[cfg(target_arch = "x86_64")]
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    Module32FirstW, Module32NextW, MODULEENTRY32W, TH32CS_SNAPMODULE, TH32CS_SNAPMODULE32,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Memory::{
    VirtualAllocEx, VirtualFreeEx, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
};
use windows::Win32::System::SystemInformation::IMAGE_FILE_MACHINE_UNKNOWN;
use windows::Win32::System::Threading::{
    CreateRemoteThread, GetCurrentProcessId, GetExitCodeThread, GetProcessId, IsWow64Process2,
    OpenProcess, WaitForSingleObject, INFINITE, PROCESS_ALL_ACCESS,
};
#[cfg(target_arch = "x86")]
use windows::Win32::UI::WindowsAndMessaging::FindWindowA;
#[cfg(target_arch = "x86")]
use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
#[cfg(target_arch = "x86_64")]
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, GetWindowThreadProcessId};

/// A process, open with the permissions appropriate for injection.
pub struct Process(HANDLE);

/// The work performed by an injection request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InjectionOutcome {
    /// Whether this request loaded the DLL, rather than reusing a loaded module.
    pub newly_loaded: bool,
    /// Whether the optional callback was found and accepted the request.
    pub callback_invoked: bool,
}

impl Process {
    /// Retrieve the process ID by window title, returning the first match, and
    /// open it with the appropriate permissions.
    pub fn by_title(title: &str) -> Result<Self> {
        unsafe { get_process_by_title(title) }.map(Self)
    }

    /// Retrieve the process ID by executable name, returning the first match,
    /// and open it with the appropriate permissions.
    pub fn by_name(name: &str) -> Result<Self> {
        unsafe { get_process_by_name(name) }.map(Self)
    }

    /// Inject the DLL in the process, reusing it if the same path is already loaded.
    pub fn inject(&self, dll_path: PathBuf) -> Result<()> {
        self.inject_inner(dll_path, None).map(|_| ())
    }

    /// Inject or reuse a DLL, then call a named export if it exists.
    ///
    /// The export must use the remote-thread ABI:
    /// `extern "system" fn(*mut c_void) -> u32`. It receives a null argument and
    /// must return 1 when it accepts the request. Any other result is reported as
    /// an error. Missing exports preserve ordinary injection behavior.
    ///
    /// This supports modules that remain mapped after stopping their hooks: the
    /// callback can request reinitialization without increasing the loader's
    /// reference count or running `DllMain` again. A callback should only queue
    /// the request, leaving initialization to the module's own worker thread.
    pub fn inject_with_optional_callback(
        &self,
        dll_path: PathBuf,
        callback_name: &str,
    ) -> Result<InjectionOutcome> {
        self.inject_inner(dll_path, Some(callback_name))
    }

    fn inject_inner(
        &self,
        dll_path: PathBuf,
        callback_name: Option<&str>,
    ) -> Result<InjectionOutcome> {
        let dll_path = dll_path.canonicalize().map_err(io_error)?;
        let file = std::fs::read(&dll_path).map_err(io_error)?;
        let image = PeImage::parse(&file)?;
        let pid = unsafe { GetProcessId(self.0) };
        if pid == 0 {
            return Err(Error::from_thread());
        }
        validate_architecture(self.0, image.machine)?;
        // Parse before loading so malformed or forwarded callbacks cannot leave
        // behind a newly loaded DLL when the request is rejected.
        if let Some(name) = callback_name {
            image.export_rva(name)?;
        }
        let modules = process_modules(pid)?;
        let existing = modules.iter().find(|module| same_path(&module.path, &dll_path));
        let newly_loaded = existing.is_none();
        let module = if let Some(module) = existing {
            module.clone()
        } else {
            let load_library = remote_load_library_address(&modules)?;
            self.load_library(&dll_path, load_library)?;
            // Thread exit codes are DWORDs and truncate HMODULE on x64. The
            // module list, not that exit code, determines whether loading worked.
            process_modules(pid)?
                .into_iter()
                .find(|module| same_path(&module.path, &dll_path))
                .ok_or_else(|| invalid("LoadLibraryW completed but the DLL was not loaded"))?
        };
        // A file at the same path can have been replaced since the resident DLL
        // was loaded. Resolve the callback from the actual remote image so an
        // old module never receives a newer build's RVA.
        let callback_rva = callback_name
            .map(|name| remote_export_rva(self.0, &module, name))
            .transpose()?
            .flatten();
        if let Some(rva) = callback_rva {
            if rva >= module.size {
                return Err(invalid("Callback lies outside the loaded DLL"));
            }
            let address = module
                .base
                .checked_add(rva as usize)
                .ok_or_else(|| invalid("Callback address overflow"))?;
            if run_remote_thread(self.0, address, std::ptr::null_mut())? != 1 {
                return Err(invalid(
                    "DLL callback rejected the request; wait for shutdown to finish and retry",
                ));
            }
        }
        Ok(InjectionOutcome { newly_loaded, callback_invoked: callback_rva.is_some() })
    }

    fn load_library(&self, dll_path: &Path, address: usize) -> Result<()> {
        let path: Vec<u16> = dll_path.as_os_str().encode_wide().chain(Some(0)).collect();
        let byte_len = path.len() * size_of::<u16>();
        let buffer = unsafe {
            VirtualAllocEx(self.0, None, byte_len, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE)
        };
        if buffer.is_null() {
            return Err(Error::from_thread());
        }
        let result = (|| {
            let mut bytes_written = 0usize;
            unsafe {
                WriteProcessMemory(
                    self.0,
                    buffer,
                    path.as_ptr().cast(),
                    byte_len,
                    Some(&mut bytes_written),
                )?;
            }
            debug!("WriteProcessMemory: written {} bytes", bytes_written);
            if bytes_written != byte_len {
                return Err(invalid("Incomplete DLL path write"));
            }
            run_remote_thread(self.0, address, buffer).map(|_| ())
        })();
        let free_result = unsafe { VirtualFreeEx(self.0, buffer, 0, MEM_RELEASE) };
        result.and(free_result)
    }

    /// Retrieve the process handle.
    pub fn handle(&self) -> HANDLE {
        self.0
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0).ok() };
    }
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0).ok() };
    }
}

#[derive(Clone)]
struct RemoteModule {
    path: PathBuf,
    base: usize,
    size: u32,
}

fn process_modules(pid: u32) -> Result<Vec<RemoteModule>> {
    let mut snapshot = None;
    // The loader can change while taking a snapshot. Toolhelp documents
    // ERROR_BAD_LENGTH as retryable for module snapshots.
    for _ in 0..5 {
        match unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid) } {
            Ok(handle) => {
                snapshot = Some(OwnedHandle(handle));
                break;
            },
            Err(error) if error.code() == ERROR_BAD_LENGTH.to_hresult() => continue,
            Err(error) => return Err(error),
        }
    }
    let snapshot = snapshot.ok_or_else(|| invalid("Module list kept changing; retry injection"))?;
    let mut entry =
        MODULEENTRY32W { dwSize: size_of::<MODULEENTRY32W>() as u32, ..Default::default() };
    if let Err(error) = unsafe { Module32FirstW(snapshot.0, &mut entry) } {
        if error.code() == ERROR_NO_MORE_FILES.to_hresult() {
            return Ok(Vec::new());
        }
        return Err(error);
    }
    let mut modules = Vec::new();
    loop {
        let length =
            entry.szExePath.iter().position(|&value| value == 0).unwrap_or(entry.szExePath.len());
        modules.push(RemoteModule {
            path: PathBuf::from(OsString::from_wide(&entry.szExePath[..length])),
            base: entry.modBaseAddr as usize,
            size: entry.modBaseSize,
        });
        match unsafe { Module32NextW(snapshot.0, &mut entry) } {
            Ok(()) => {},
            Err(error) if error.code() == ERROR_NO_MORE_FILES.to_hresult() => break,
            Err(error) => return Err(error),
        }
    }
    Ok(modules)
}

fn normalized_path(path: &Path) -> String {
    let value = path.to_string_lossy().replace('/', "\\");
    let value = if let Some(unc) = value.strip_prefix("\\\\?\\UNC\\") {
        format!("\\\\{unc}")
    } else {
        value.strip_prefix("\\\\?\\").unwrap_or(&value).to_owned()
    };
    value.to_lowercase()
}

fn same_path(left: &Path, right: &Path) -> bool {
    normalized_path(left) == normalized_path(right)
}

fn validate_architecture(process: HANDLE, dll_machine: u16) -> Result<()> {
    #[cfg(target_arch = "x86")]
    let injector_machine = 0x014c;
    #[cfg(target_arch = "x86_64")]
    let injector_machine = 0x8664;
    #[cfg(target_arch = "aarch64")]
    let injector_machine = 0xaa64;
    let mut process_machine = IMAGE_FILE_MACHINE_UNKNOWN;
    let mut native_machine = IMAGE_FILE_MACHINE_UNKNOWN;
    unsafe { IsWow64Process2(process, &mut process_machine, Some(&mut native_machine))? };
    let target_machine = if process_machine == IMAGE_FILE_MACHINE_UNKNOWN {
        native_machine.0
    } else {
        process_machine.0
    };
    if dll_machine != injector_machine || target_machine != injector_machine {
        return Err(invalid("Injector, DLL, and target process architectures must match"));
    }
    Ok(())
}

fn remote_load_library_address(remote_modules: &[RemoteModule]) -> Result<usize> {
    let address = unsafe { GetProcAddress(GetModuleHandleW(w!("Kernel32"))?, s!("LoadLibraryW")) }
        .ok_or_else(|| invalid("LoadLibraryW was not found"))? as usize;
    // GetProcAddress can resolve forwarded exports into KernelBase. Locate the
    // actual containing module, then translate its RVA into the target process.
    let local_modules = process_modules(unsafe { GetCurrentProcessId() })?;
    let local = local_modules
        .iter()
        .find(|module| address >= module.base && address - module.base < module.size as usize)
        .ok_or_else(|| invalid("Could not locate the LoadLibraryW module"))?;
    let remote = remote_modules
        .iter()
        .find(|module| {
            module.path.file_name().zip(local.path.file_name()).is_some_and(|(left, right)| {
                left.to_string_lossy().eq_ignore_ascii_case(&right.to_string_lossy())
            })
        })
        .ok_or_else(|| invalid("The target does not contain the LoadLibraryW module"))?;
    let offset = address - local.base;
    if offset >= remote.size as usize {
        return Err(invalid("LoadLibraryW lies outside its remote module"));
    }
    remote.base.checked_add(offset).ok_or_else(|| invalid("LoadLibraryW address overflow"))
}

fn run_remote_thread(process: HANDLE, address: usize, argument: *mut c_void) -> Result<u32> {
    let callback =
        unsafe { mem::transmute::<usize, unsafe extern "system" fn(*mut c_void) -> u32>(address) };
    let thread = OwnedHandle(unsafe {
        CreateRemoteThread(process, None, 0, Some(callback), Some(argument), 0, None)?
    });
    if unsafe { WaitForSingleObject(thread.0, INFINITE) } != WAIT_OBJECT_0 {
        return Err(Error::from_thread());
    }
    let mut exit_code = 0;
    unsafe { GetExitCodeThread(thread.0, &mut exit_code)? };
    Ok(exit_code)
}

fn remote_export_rva(process: HANDLE, module: &RemoteModule, name: &str) -> Result<Option<u32>> {
    let read = |rva: u32, length: usize| -> Result<Vec<u8>> {
        let end = (rva as usize)
            .checked_add(length)
            .ok_or_else(|| invalid("Remote image read overflow"))?;
        if end > module.size as usize {
            return Err(invalid("Remote image read is outside the module"));
        }
        let address = module
            .base
            .checked_add(rva as usize)
            .ok_or_else(|| invalid("Remote image address overflow"))?;
        let mut bytes = vec![0; length];
        let mut bytes_read = 0;
        unsafe {
            ReadProcessMemory(
                process,
                address as *const c_void,
                bytes.as_mut_ptr().cast(),
                length,
                Some(&mut bytes_read),
            )?;
        }
        if bytes_read != length {
            return Err(invalid("Incomplete remote image read"));
        }
        Ok(bytes)
    };
    let dos = read(0, 64)?;
    let nt = read_u32(&dos, 0x3c)?;
    let nt_header = read(nt, 24)?;
    let section_count = read_u16(&nt_header, 6)? as usize;
    let optional_size = read_u16(&nt_header, 20)? as usize;
    let header_length = checked_offset(nt as usize, 24)?;
    let header_length = checked_offset(header_length, optional_size)?;
    let header_length = checked_offset(header_length, section_count * 40)?;
    let headers = read(0, header_length)?;
    let image = PeImage::parse(&headers)?;
    let Some((directory_rva, directory_size)) = image.export_directory else {
        return Ok(None);
    };
    let directory = read(directory_rva, 40)?;
    let rva = find_export_rva(directory_rva, directory_size, &directory, name, read)?;
    if let Some(rva) = rva {
        image.validate_executable_rva(rva)?;
    }
    Ok(rva)
}

fn invalid(message: &str) -> Error {
    Error::new(HRESULT(0x80004005u32 as i32), message)
}

fn io_error(error: std::io::Error) -> Error {
    Error::new(HRESULT(0x80004005u32 as i32), error.to_string())
}

struct PeSection {
    virtual_address: u32,
    raw_size: u32,
    raw_offset: u32,
    characteristics: u32,
}

struct PeImage<'a> {
    bytes: &'a [u8],
    machine: u16,
    header_size: u32,
    export_directory: Option<(u32, u32)>,
    sections: Vec<PeSection>,
}

impl<'a> PeImage<'a> {
    fn parse(bytes: &'a [u8]) -> Result<Self> {
        if read_u16(bytes, 0)? != 0x5a4d {
            return Err(invalid("Invalid DOS header"));
        }
        let nt = read_u32(bytes, 0x3c)? as usize;
        if read_u32(bytes, nt)? != 0x0000_4550 {
            return Err(invalid("Invalid PE signature"));
        }
        let coff = checked_offset(nt, 4)?;
        let machine = read_u16(bytes, coff)?;
        let section_count = read_u16(bytes, checked_offset(coff, 2)?)? as usize;
        let optional_size = read_u16(bytes, checked_offset(coff, 16)?)? as usize;
        let optional = checked_offset(coff, 20)?;
        let (directory_count_offset, directory_offset) = match read_u16(bytes, optional)? {
            0x10b if machine == 0x014c => (92, 96),
            0x20b if matches!(machine, 0x8664 | 0xaa64) => (108, 112),
            _ => return Err(invalid("Unsupported or inconsistent PE architecture")),
        };
        if optional_size < directory_offset {
            return Err(invalid("Truncated PE optional header"));
        }
        let header_size = read_u32(bytes, checked_offset(optional, 60)?)?;
        let directory_count = read_u32(bytes, checked_offset(optional, directory_count_offset)?)?;
        let export_directory = if directory_count != 0 {
            if optional_size < directory_offset + 8 {
                return Err(invalid("Truncated PE export directory entry"));
            }
            let rva = read_u32(bytes, checked_offset(optional, directory_offset)?)?;
            let size = read_u32(bytes, checked_offset(optional, directory_offset + 4)?)?;
            (rva != 0 && size != 0).then_some((rva, size))
        } else {
            None
        };
        let section_table = checked_offset(optional, optional_size)?;
        let mut sections = Vec::with_capacity(section_count);
        for index in 0..section_count {
            let section = checked_offset(section_table, index * 40)?;
            sections.push(PeSection {
                virtual_address: read_u32(bytes, checked_offset(section, 12)?)?,
                raw_size: read_u32(bytes, checked_offset(section, 16)?)?,
                raw_offset: read_u32(bytes, checked_offset(section, 20)?)?,
                characteristics: read_u32(bytes, checked_offset(section, 36)?)?,
            });
        }
        Ok(Self { bytes, machine, header_size, export_directory, sections })
    }

    fn rva_offset(&self, rva: u32) -> Result<usize> {
        let offset = if rva < self.header_size {
            rva as usize
        } else {
            let section = self
                .sections
                .iter()
                .find(|section| {
                    rva >= section.virtual_address
                        && rva - section.virtual_address < section.raw_size
                })
                .ok_or_else(|| invalid("Unmapped PE RVA"))?;
            section
                .raw_offset
                .checked_add(rva - section.virtual_address)
                .ok_or_else(|| invalid("PE RVA overflow"))? as usize
        };
        if offset >= self.bytes.len() {
            return Err(invalid("PE RVA is outside the file"));
        }
        Ok(offset)
    }

    fn export_rva(&self, name: &str) -> Result<Option<u32>> {
        let Some((directory_rva, directory_size)) = self.export_directory else {
            return Ok(None);
        };
        let directory = self.rva_offset(directory_rva)?;
        let end = checked_offset(directory, 40)?;
        let directory = self
            .bytes
            .get(directory..end)
            .ok_or_else(|| invalid("Truncated PE export directory"))?;
        let rva =
            find_export_rva(directory_rva, directory_size, directory, name, |rva, length| {
                let offset = self.rva_offset(rva)?;
                let end = checked_offset(offset, length)?;
                self.bytes
                    .get(offset..end)
                    .map(<[u8]>::to_vec)
                    .ok_or_else(|| invalid("Truncated PE export table"))
            })?;
        if let Some(rva) = rva {
            self.validate_executable_rva(rva)?;
            self.rva_offset(rva)?;
        }
        Ok(rva)
    }

    fn validate_executable_rva(&self, rva: u32) -> Result<()> {
        if !self.sections.iter().any(|section| {
            rva >= section.virtual_address
                && rva - section.virtual_address < section.raw_size
                && section.characteristics & 0x2000_0000 != 0
        }) {
            return Err(invalid("PE callback does not point to executable code"));
        }
        Ok(())
    }
}

fn find_export_rva(
    directory_rva: u32,
    directory_size: u32,
    directory: &[u8],
    name: &str,
    mut read: impl FnMut(u32, usize) -> Result<Vec<u8>>,
) -> Result<Option<u32>> {
    if directory_size < 40 {
        return Err(invalid("Truncated PE export directory"));
    }
    let directory_end = directory_rva
        .checked_add(directory_size)
        .ok_or_else(|| invalid("PE export directory overflow"))?;
    let function_count = read_u32(directory, 20)?;
    let name_count = read_u32(directory, 24)?;
    let functions = read_u32(directory, 28)?;
    let names = read_u32(directory, 32)?;
    let ordinals = read_u32(directory, 36)?;
    for index in 0..name_count {
        let name_rva = read_u32(&read(checked_rva(names, index, 4)?, 4)?, 0)?;
        let mut matches = true;
        // Read one byte at a time so a short export name at the end of a mapped
        // page never causes a speculative read into an inaccessible next page.
        for (offset, expected) in name.bytes().chain(Some(0)).enumerate() {
            let offset = u32::try_from(offset).map_err(|_| invalid("Export name is too long"))?;
            if read(checked_rva(name_rva, offset, 1)?, 1)?[0] != expected {
                matches = false;
                break;
            }
        }
        if !matches {
            continue;
        }
        let ordinal = read_u16(&read(checked_rva(ordinals, index, 2)?, 2)?, 0)?;
        if u32::from(ordinal) >= function_count {
            return Err(invalid("Invalid PE export ordinal"));
        }
        let rva = read_u32(&read(checked_rva(functions, u32::from(ordinal), 4)?, 4)?, 0)?;
        if (directory_rva..directory_end).contains(&rva) {
            return Err(invalid("Forwarded callbacks are unsupported"));
        }
        return Ok(Some(rva));
    }
    Ok(None)
}

fn checked_offset(base: usize, offset: usize) -> Result<usize> {
    base.checked_add(offset).ok_or_else(|| invalid("PE file offset overflow"))
}

fn checked_rva(base: u32, index: u32, stride: u32) -> Result<u32> {
    index
        .checked_mul(stride)
        .and_then(|offset| base.checked_add(offset))
        .ok_or_else(|| invalid("PE table RVA overflow"))
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    let end = checked_offset(offset, 2)?;
    let value = bytes.get(offset..end).ok_or_else(|| invalid("Truncated PE file"))?;
    Ok(u16::from_le_bytes([value[0], value[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let end = checked_offset(offset, 4)?;
    let value = bytes.get(offset..end).ok_or_else(|| invalid("Truncated PE file"))?;
    Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

// Find process given the title of one of its windows.
// 32-bit implementation. Uses [`std::ffi::CString`] and `FindWindowA`.
#[cfg(target_arch = "x86")]
unsafe fn get_process_by_title(title: &str) -> Result<HANDLE> {
    let title = HSTRING::from(title).to_os_string();
    let hwnd = FindWindowA(None, PCSTR(title.as_encoded_bytes().as_ptr()))?;

    let mut pid: u32 = 0;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));

    OpenProcess(PROCESS_ALL_ACCESS, false, pid)
}

// Find process given the title of one of its windows.
// 64-bit implementation. Uses [`widestring::U16CString`] and `FindWindowW`.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
unsafe fn get_process_by_title(title: &str) -> Result<HANDLE> {
    let title = HSTRING::from(title);
    let hwnd = FindWindowW(None, PCWSTR(title.as_ptr()))?;

    let mut pid: u32 = 0;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));

    OpenProcess(PROCESS_ALL_ACCESS, false, pid)
}

// Find process given the process name.
// 32-bit implementation. Uses [`PROCESSENTRY32`].
#[cfg(target_arch = "x86")]
unsafe fn get_process_by_name(name_str: &str) -> Result<HANDLE> {
    let name = HSTRING::from(name_str).to_os_string();
    let name = name.as_encoded_bytes();

    let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)?;
    let mut pe32 =
        PROCESSENTRY32 { dwSize: mem::size_of::<PROCESSENTRY32>() as u32, ..Default::default() };

    if let Err(e) = Process32First(snapshot, &mut pe32) {
        CloseHandle(snapshot).ok();
        return Err(e);
    }

    let pid = loop {
        let zero_idx = pe32.szExeFile.iter().position(|&x| x == 0).unwrap_or(pe32.szExeFile.len());
        let proc_name = &pe32.szExeFile[..zero_idx];

        if proc_name.iter().map(|&x| x as u8).eq(name.iter().copied()) {
            break Ok(pe32.th32ProcessID);
        }

        if Process32Next(snapshot, &mut pe32).is_err() {
            CloseHandle(snapshot).ok();
            break Err(Error::from_hresult(HRESULT(-1)));
        }
    }?;

    CloseHandle(snapshot)?;

    OpenProcess(PROCESS_ALL_ACCESS, false, pid)
}

// Find process given the process name.
// 64-bit implementation. Uses [`PROCESSENTRY32W`].
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
unsafe fn get_process_by_name(name_str: &str) -> Result<HANDLE> {
    let name = HSTRING::from(name_str);

    let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)?;
    let mut pe32 =
        PROCESSENTRY32W { dwSize: mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };

    if let Err(e) = Process32FirstW(snapshot, &mut pe32) {
        CloseHandle(snapshot).ok();
        return Err(e);
    }

    let pid = loop {
        let zero_idx = pe32.szExeFile.iter().position(|&x| x == 0).unwrap_or(pe32.szExeFile.len());
        let proc_name = HSTRING::from_wide(&pe32.szExeFile[..zero_idx]);

        if name == proc_name {
            break Ok(pe32.th32ProcessID);
        }

        if Process32NextW(snapshot, &mut pe32).is_err() {
            CloseHandle(snapshot).ok();
            break Err(Error::from_hresult(HRESULT(-1)));
        }
    }?;

    CloseHandle(snapshot)?;

    OpenProcess(PROCESS_ALL_ACCESS, false, pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_u16(bytes: &mut [u8], offset: usize, value: u16) {
        bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn set_u32(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn fixture(is_64_bit: bool) -> Vec<u8> {
        let mut bytes = vec![0; 0x800];
        set_u16(&mut bytes, 0, 0x5a4d);
        set_u32(&mut bytes, 0x3c, 0x80);
        set_u32(&mut bytes, 0x80, 0x4550);
        set_u16(&mut bytes, 0x84, if is_64_bit { 0x8664 } else { 0x014c });
        set_u16(&mut bytes, 0x86, 1);
        let optional_size = if is_64_bit { 240 } else { 224 };
        set_u16(&mut bytes, 0x94, optional_size);
        let optional = 0x98;
        set_u16(&mut bytes, optional, if is_64_bit { 0x20b } else { 0x10b });
        set_u32(&mut bytes, optional + 60, 0x200);
        let directory_offset = if is_64_bit { 112 } else { 96 };
        set_u32(&mut bytes, optional + directory_offset - 4, 16);
        set_u32(&mut bytes, optional + directory_offset, 0x1000);
        set_u32(&mut bytes, optional + directory_offset + 4, 0x100);
        let section = optional + optional_size as usize;
        set_u32(&mut bytes, section + 12, 0x1000);
        set_u32(&mut bytes, section + 16, 0x600);
        set_u32(&mut bytes, section + 20, 0x200);
        set_u32(&mut bytes, section + 36, 0x2000_0000);
        // Export directory and its three tables, all in the section at RVA 0x1000.
        set_u32(&mut bytes, 0x214, 1);
        set_u32(&mut bytes, 0x218, 1);
        set_u32(&mut bytes, 0x21c, 0x1040);
        set_u32(&mut bytes, 0x220, 0x1044);
        set_u32(&mut bytes, 0x224, 0x1048);
        set_u32(&mut bytes, 0x240, 0x1200);
        set_u32(&mut bytes, 0x244, 0x1050);
        let name = b"L4D2_RequestResume\0";
        bytes[0x250..0x250 + name.len()].copy_from_slice(name);
        bytes
    }

    #[test]
    fn resolves_callback_rva_for_both_pe_architectures() {
        for is_64_bit in [false, true] {
            let bytes = fixture(is_64_bit);
            let image = PeImage::parse(&bytes).unwrap();
            assert_eq!(image.machine, if is_64_bit { 0x8664 } else { 0x014c });
            assert_eq!(image.export_rva("L4D2_RequestResume").unwrap(), Some(0x1200));
            assert_eq!(image.export_rva("Absent").unwrap(), None);
        }
    }

    #[test]
    fn rejects_forwarded_or_non_executable_callbacks() {
        let mut bytes = fixture(false);
        set_u32(&mut bytes, 0x240, 0x1060);
        assert!(PeImage::parse(&bytes).unwrap().export_rva("L4D2_RequestResume").is_err());
        set_u32(&mut bytes, 0x240, 0x1200);
        set_u32(&mut bytes, 0x98 + 224 + 36, 0);
        assert!(PeImage::parse(&bytes).unwrap().export_rva("L4D2_RequestResume").is_err());
    }

    #[test]
    fn rejects_truncated_pe_and_invalid_ordinals() {
        assert!(PeImage::parse(&[]).is_err());
        let mut bytes = fixture(true);
        set_u16(&mut bytes, 0x248, 1);
        assert!(PeImage::parse(&bytes).unwrap().export_rva("L4D2_RequestResume").is_err());
        set_u32(&mut bytes, 0x220, u32::MAX);
        assert!(PeImage::parse(&bytes).unwrap().export_rva("L4D2_RequestResume").is_err());
    }

    #[test]
    fn missing_export_directory_preserves_normal_injection() {
        let mut bytes = fixture(false);
        set_u32(&mut bytes, 0x98 + 96, 0);
        assert_eq!(PeImage::parse(&bytes).unwrap().export_rva("L4D2_RequestResume").unwrap(), None);
    }

    #[test]
    fn compares_canonical_extended_windows_paths() {
        assert!(same_path(
            Path::new(r"\\?\D:\Some Dir\test.dll"),
            Path::new(r"d:\some dir\TEST.DLL")
        ));
        assert!(same_path(
            Path::new(r"\\?\UNC\server\share\test.dll"),
            Path::new(r"\\SERVER\share\test.dll")
        ));
        assert!(!same_path(Path::new(r"D:\A\test.dll"), Path::new(r"D:\B\test.dll")));
    }
}
