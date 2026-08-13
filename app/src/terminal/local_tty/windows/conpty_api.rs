use std::mem::transmute;
use std::path::Path;

use thiserror::Error;
use warp_util::path::TargetDirError;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Console::{COORD, HPCON};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::core::{HRESULT, HSTRING, PCWSTR, s};
use winreg::RegKey;
use winreg::enums::HKEY_LOCAL_MACHINE;

const BUNDLED_CONPTY_DLL: &str = "conpty.dll";
const SYSTEM_CONPTY_DLL: &str = "kernel32.dll";
const BUNDLED_CONPTY_MIN_WINDOWS_BUILD: u32 = 18362;

const CREATE_PSUEDOCONSOLE_FN_NAME: &str = "CreatePsuedoConsole";
const RESIZE_PSUEDOCONSOLE_FN_NAME: &str = "ResizePsuedoConsole";
const CLOSE_PSUEDOCONSOLE_FN_NAME: &str = "ClosePsuedoConsole";
const SHOW_HIDE_PSUEDOCONSOLE_FN_NAME: &str = "ShowHidePseudoConsole";
const RELEASE_PSUEDOCONSOLE_FN_NAME: &str = "ReleasePseudoConsole";

type CreatePseudoConsoleFn =
    unsafe extern "system" fn(COORD, HANDLE, HANDLE, u32, *mut HPCON) -> HRESULT;
type ResizePseudoConsoleFn = unsafe extern "system" fn(HPCON, COORD) -> HRESULT;
type ClosePseudoConsoleFn = unsafe extern "system" fn(HPCON);
type ShowHidePseudoConsoleFn = unsafe extern "system" fn(HPCON, bool) -> HRESULT;
type ReleasePseudoConsoleFn = unsafe extern "system" fn(HPCON) -> HRESULT;

struct SendablePseudoConsole(HPCON);

// A pseudoconsole handle can be closed from a thread other than the one that created it.
unsafe impl Send for SendablePseudoConsole {}

impl SendablePseudoConsole {
    unsafe fn close(self, close: ClosePseudoConsoleFn) {
        unsafe { close(self.0) }
    }
}

pub struct ConptyApi {
    /// Function pointer for CreatePseudoConsole.
    create: CreatePseudoConsoleFn,
    /// Function pointer for ResizePseudoConsole.
    resize: ResizePseudoConsoleFn,
    /// Function pointer for ClosePseudoConsole.
    close: ClosePseudoConsoleFn,
    /// Optional side-by-side ConPTY extension for syncing pseudo-window visibility.
    show_hide: Option<ShowHidePseudoConsoleFn>,
    /// Optional side-by-side ConPTY extension for releasing the reference handle.
    release: Option<ReleasePseudoConsoleFn>,
    /// Whether closing must account for legacy system ConPTY blocking behavior.
    system_backend: bool,
}

#[derive(Error, Debug)]
pub enum ConptyApiError {
    #[error("Failed to construct target directory: {0}")]
    NoTargetDirectory(#[from] TargetDirError),
    #[error(
        "Failed to load ConPTY library module {module_name:?}: {windows_error:#}. DLL file exists: {dll_file_exists:?}"
    )]
    LoadLibraryFailed {
        module_name: String,
        #[source]
        windows_error: windows::core::Error,
        dll_file_exists: Result<bool, std::io::Error>,
    },
    #[error("Failed to get procedure address for {fn_name:?}")]
    GetProcAddressFailed { fn_name: String },
}

fn windows_build_number() -> Option<u32> {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let current_version = hklm
        .open_subkey(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion")
        .ok()?;
    let build_number: String = current_version.get_value("CurrentBuildNumber").ok()?;
    build_number.parse().ok()
}

pub(super) fn requires_synchronous_conpty_pipe() -> bool {
    windows_build_number().is_some_and(|build| build < BUNDLED_CONPTY_MIN_WINDOWS_BUILD)
}

impl ConptyApi {
    pub(super) unsafe fn load() -> Result<Self, ConptyApiError> {
        let windows_build = windows_build_number();

        if let Some(build) =
            windows_build.filter(|build| *build < BUNDLED_CONPTY_MIN_WINDOWS_BUILD)
        {
            log::info!("Using system ConPTY compatibility backend for Windows build {build}");
            match unsafe { Self::load_from_module(SYSTEM_CONPTY_DLL, false) } {
                Ok(api) => return Ok(api),
                Err(system_error) => {
                    log::warn!(
                        "Failed to load the system ConPTY compatibility backend; trying bundled ConPTY: {system_error:#}"
                    );
                }
            }
        }

        match unsafe { Self::load_from_module(BUNDLED_CONPTY_DLL, true) } {
            Ok(api) => {
                log::info!("Using bundled ConPTY backend for Windows build {windows_build:?}");
                Ok(api)
            }
            Err(bundled_error) => {
                log::warn!(
                    "Failed to load bundled ConPTY; falling back to the system ConPTY backend: {bundled_error:#}"
                );
                unsafe { Self::load_from_module(SYSTEM_CONPTY_DLL, false) }
            }
        }
    }

    unsafe fn load_from_module(
        module_name: &str,
        require_extensions: bool,
    ) -> Result<Self, ConptyApiError> {
        type LoadedFn = unsafe extern "system" fn() -> isize;

        let hstring = HSTRING::from(module_name);
        let dll_file_path = PCWSTR::from_raw(hstring.as_ptr());
        let module = match unsafe { LoadLibraryW(dll_file_path) } {
            Ok(module) => module,
            Err(windows_error) => {
                let dll_file_exists = if module_name == BUNDLED_CONPTY_DLL {
                    Path::new("./conpty.dll").try_exists()
                } else {
                    Ok(true)
                };
                return Err(ConptyApiError::LoadLibraryFailed {
                    module_name: module_name.to_owned(),
                    windows_error,
                    dll_file_exists,
                });
            }
        };

        let Some(create) = unsafe { GetProcAddress(module, s!("CreatePseudoConsole")) }
            .map(|create_fn| unsafe { transmute::<LoadedFn, CreatePseudoConsoleFn>(create_fn) })
        else {
            return Err(ConptyApiError::GetProcAddressFailed {
                fn_name: CREATE_PSUEDOCONSOLE_FN_NAME.to_string(),
            });
        };
        let Some(resize) = unsafe { GetProcAddress(module, s!("ResizePseudoConsole")) }
            .map(|resize_fn| unsafe { transmute::<LoadedFn, ResizePseudoConsoleFn>(resize_fn) })
        else {
            return Err(ConptyApiError::GetProcAddressFailed {
                fn_name: RESIZE_PSUEDOCONSOLE_FN_NAME.to_string(),
            });
        };
        let Some(close) = unsafe { GetProcAddress(module, s!("ClosePseudoConsole")) }
            .map(|close_fn| unsafe { transmute::<LoadedFn, ClosePseudoConsoleFn>(close_fn) })
        else {
            return Err(ConptyApiError::GetProcAddressFailed {
                fn_name: CLOSE_PSUEDOCONSOLE_FN_NAME.to_string(),
            });
        };

        let show_hide = unsafe { GetProcAddress(module, s!("ConptyShowHidePseudoConsole")) }.map(
            |show_hide_fn| unsafe { transmute::<LoadedFn, ShowHidePseudoConsoleFn>(show_hide_fn) },
        );
        if require_extensions && show_hide.is_none() {
            return Err(ConptyApiError::GetProcAddressFailed {
                fn_name: SHOW_HIDE_PSUEDOCONSOLE_FN_NAME.to_string(),
            });
        }

        let release = unsafe { GetProcAddress(module, s!("ConptyReleasePseudoConsole")) }.map(
            |release_fn| unsafe { transmute::<LoadedFn, ReleasePseudoConsoleFn>(release_fn) },
        );
        if require_extensions && release.is_none() {
            return Err(ConptyApiError::GetProcAddressFailed {
                fn_name: RELEASE_PSUEDOCONSOLE_FN_NAME.to_string(),
            });
        }

        Ok(ConptyApi {
            create,
            resize,
            close,
            show_hide,
            release,
            system_backend: module_name == SYSTEM_CONPTY_DLL,
        })
    }

    pub(super) unsafe fn create(
        &self,
        size: COORD,
        mut pipe: HANDLE,
        flags: u32,
    ) -> Result<HPCON, windows::core::Error> {
        let mut pty_handle = HPCON::default();
        let result = unsafe { (self.create)(size, pipe, pipe, flags, &mut pty_handle) }
            .ok()
            .map(|_| pty_handle);
        // Explicitly free our end of the pipe, giving the pseudoconsole sole
        // ownership of it.
        unsafe {
            windows::core::Free::free(&mut pipe);
        }
        result
    }

    pub(super) unsafe fn resize(
        &self,
        pty_handle: HPCON,
        size: COORD,
    ) -> Result<(), windows::core::Error> {
        unsafe { (self.resize)(pty_handle, size).ok() }
    }

    pub(super) unsafe fn close(&self, pty_handle: HPCON) {
        if self.system_backend {
            let close = self.close;
            let pty_handle = SendablePseudoConsole(pty_handle);
            let _ = std::thread::spawn(move || unsafe {
                pty_handle.close(close);
            });
        } else {
            unsafe { (self.close)(pty_handle) }
        }
    }

    pub(super) unsafe fn show_hide(
        &self,
        pty_handle: HPCON,
        visible: bool,
    ) -> windows::core::Result<()> {
        match self.show_hide {
            Some(show_hide) => unsafe { show_hide(pty_handle, visible).ok() },
            None => Ok(()),
        }
    }

    pub(super) unsafe fn release(&self, pty_handle: HPCON) -> windows::core::Result<()> {
        match self.release {
            Some(release) => unsafe { release(pty_handle).ok() },
            None => Ok(()),
        }
    }
}
