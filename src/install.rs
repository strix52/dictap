//! Per-user install, no admin needed: the exe goes to `%LOCALAPPDATA%\Programs\<name>`, with
//! a Start menu shortcut (what Start search and launchers such as Raycast index), an entry in
//! Settings > Apps for uninstalling, and the Run key so the hotkey works after sign-in.

use crate::NAME;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, IPersistFile,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ,
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW,
};
use windows::Win32::System::Threading::{OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE};
use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
use windows::Win32::UI::WindowsAndMessaging::{
    IDYES, MB_DEFBUTTON2, MB_ICONERROR, MB_ICONINFORMATION, MB_ICONQUESTION, MB_OK, MB_YESNO,
    MESSAGEBOX_STYLE, MessageBoxW,
};
use windows::core::{HSTRING, Interface};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).map(PathBuf::from)
}

/// Where the installed exe lives.
pub fn dir() -> Option<PathBuf> {
    Some(env_dir("LOCALAPPDATA")?.join("Programs").join(NAME))
}

fn installed_exe() -> Option<PathBuf> {
    Some(dir()?.join(format!("{NAME}.exe")))
}

fn shortcut() -> Option<PathBuf> {
    Some(
        env_dir("APPDATA")?
            .join(r"Microsoft\Windows\Start Menu\Programs")
            .join(format!("{NAME}.lnk")),
    )
}

fn uninstall_key() -> String {
    format!(r"Software\Microsoft\Windows\CurrentVersion\Uninstall\{NAME}")
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a.as_os_str().eq_ignore_ascii_case(b.as_os_str()),
        _ => false,
    }
}

/// Whether this process is the installed copy.
pub fn is_installed_copy() -> bool {
    match (std::env::current_exe(), installed_exe()) {
        (Ok(me), Some(inst)) => same_file(&me, &inst),
        _ => false,
    }
}

fn message(text: &str, style: MESSAGEBOX_STYLE) -> bool {
    // SAFETY: modal box with owned strings.
    unsafe { MessageBoxW(None, &HSTRING::from(text), &HSTRING::from(NAME), style) == IDYES }
}

/// Handles `--install` and `--uninstall`. Returns true when the process should exit.
pub fn handle_args() -> bool {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |a: &str| args.iter().any(|x| x == a);
    if has("--install") {
        if let Err(e) = install(!has("--no-launch")) {
            message(
                &format!("Couldn't install {NAME}: {e}"),
                MB_OK | MB_ICONERROR,
            );
        }
        return true;
    }
    if has("--uninstall") {
        uninstall(has("--quiet"));
        return true;
    }
    false
}

/// On a copy that isn't installed (say, just downloaded), offers to install it. Returns true
/// when the process should exit because the installed copy has taken over.
pub fn offer() -> bool {
    if cfg!(debug_assertions) || is_installed_copy() || std::env::args().any(|a| a == "--portable")
    {
        return false;
    }
    let update = installed_exe().is_some_and(|p| p.exists());
    let text = if update {
        format!(
            "Replace your installed {NAME} with this copy?\n\n\
             Your history, settings and API key stay as they are.\n\n\
             Choose No to run this copy once without installing it."
        )
    } else {
        format!(
            "Install {NAME}?\n\n\
             It goes in your Start menu and starts when you sign in, so the hotkey always \
             works. No admin rights needed; uninstall any time from Settings > Apps.\n\n\
             Choose No to run it once from here without installing."
        )
    };
    if !message(&text, MB_YESNO | MB_ICONQUESTION) {
        return false;
    }
    match install(true) {
        Ok(()) => {
            // Open Settings in the new copy so the next step (the API key) is right there.
            show_when_running(crate::win::app::Page::Settings);
            true
        }
        Err(e) => {
            message(
                &format!("Couldn't install {NAME}: {e}\n\nRunning it from here instead."),
                MB_OK | MB_ICONERROR,
            );
            false
        }
    }
}

fn show_when_running(page: crate::win::app::Page) {
    let until = Instant::now() + Duration::from_secs(8);
    while Instant::now() < until {
        if crate::win::ipc::signal_existing(page) {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn install(launch: bool) -> Result<(), String> {
    let dir = dir().ok_or("LOCALAPPDATA isn't set")?;
    let exe = installed_exe().ok_or("LOCALAPPDATA isn't set")?;
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if !same_file(&me, &exe) {
        stop_running();
        copy_retrying(&me, &exe)?;
    }
    make_shortcut(&exe, &dir).map_err(|e| format!("Start menu shortcut: {e}"))?;
    register(&exe, &dir).map_err(|e| format!("Settings > Apps entry: {e}"))?;
    crate::autostart::set_exe(Some(&exe))?;
    if launch {
        std::process::Command::new(&exe)
            .current_dir(&dir)
            .spawn()
            .map_err(|e| format!("starting {}: {e}", exe.display()))?;
    }
    Ok(())
}

/// The running instance may still be exiting (it waits briefly for a transcription).
fn copy_retrying(from: &Path, to: &Path) -> Result<(), String> {
    let mut last = String::new();
    for _ in 0..40 {
        match std::fs::copy(from, to) {
            Ok(_) => return Ok(()),
            Err(e) => last = e.to_string(),
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(format!("copying to {}: {last}", to.display()))
}

/// Asks a running instance to quit and waits (up to 10 s) until it has.
fn stop_running() {
    if !crate::win::ipc::close_existing() {
        return;
    }
    let until = Instant::now() + Duration::from_secs(10);
    while Instant::now() < until {
        // SAFETY: plain open by name; the handle is closed right away.
        match unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, crate::INSTANCE_MUTEX) } {
            Ok(h) => {
                let _ = unsafe { CloseHandle(h) };
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => return,
        }
    }
}

fn make_shortcut(exe: &Path, dir: &Path) -> windows::core::Result<()> {
    let lnk = shortcut().ok_or_else(windows::core::Error::empty)?;
    // SAFETY: COM on this thread for one short-lived object; all strings are owned HSTRINGs.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        link.SetPath(&HSTRING::from(exe.as_os_str()))?;
        link.SetWorkingDirectory(&HSTRING::from(dir.as_os_str()))?;
        link.SetDescription(&HSTRING::from(env!("CARGO_PKG_DESCRIPTION")))?;
        link.SetIconLocation(&HSTRING::from(exe.as_os_str()), 0)?;
        link.cast::<IPersistFile>()?
            .Save(&HSTRING::from(lnk.as_os_str()), true)
    }
}

enum Val<'a> {
    Str(&'a str),
    Dword(u32),
}

fn register(exe: &Path, dir: &Path) -> windows::core::Result<()> {
    let exe_s = exe.display().to_string();
    let size_kb = std::fs::metadata(exe).map_or(0, |m| (m.len() / 1024) as u32);
    let uninstall = format!("\"{exe_s}\" --uninstall");
    let quiet = format!("\"{exe_s}\" --uninstall --quiet");
    let dir_s = dir.display().to_string();
    let values = [
        ("DisplayName", Val::Str(NAME)),
        ("DisplayVersion", Val::Str(env!("CARGO_PKG_VERSION"))),
        ("DisplayIcon", Val::Str(&exe_s)),
        ("Publisher", Val::Str(NAME)),
        ("InstallLocation", Val::Str(&dir_s)),
        ("UninstallString", Val::Str(&uninstall)),
        ("QuietUninstallString", Val::Str(&quiet)),
        ("EstimatedSize", Val::Dword(size_kb)),
        ("NoModify", Val::Dword(1)),
        ("NoRepair", Val::Dword(1)),
    ];
    let mut key = HKEY::default();
    // SAFETY: creating/opening our own key under HKCU; closed below.
    unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            &HSTRING::from(uninstall_key()),
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
        .ok()?;
    }
    let mut result = Ok(());
    for (name, v) in values {
        let bytes: Vec<u8> = match v {
            Val::Str(s) => s
                .encode_utf16()
                .chain([0])
                .flat_map(u16::to_le_bytes)
                .collect(),
            Val::Dword(d) => d.to_le_bytes().to_vec(),
        };
        let ty = match v {
            Val::Str(_) => REG_SZ,
            Val::Dword(_) => REG_DWORD,
        };
        // SAFETY: open key; the buffer is valid for the call.
        let r = unsafe { RegSetValueExW(key, &HSTRING::from(name), None, ty, Some(&bytes)) };
        if r.is_err() {
            result = r.ok();
            break;
        }
    }
    // SAFETY: closing the key opened above.
    let _ = unsafe { RegCloseKey(key) };
    result
}

fn uninstall(quiet: bool) {
    if !quiet
        && !message(
            &format!("Uninstall {NAME}?"),
            MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2,
        )
    {
        return;
    }
    stop_running();
    let _ = crate::autostart::set_exe(None);
    if let Some(lnk) = shortcut() {
        let _ = std::fs::remove_file(lnk);
    }
    // SAFETY: deleting our own key under HKCU.
    let _ = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(uninstall_key())) };

    let wipe = !quiet
        && message(
            "Also delete your history, dictionary, settings and saved API key?\n\n\
             Choose No to keep them in case you install again.",
            MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2,
        );
    if wipe {
        for var in ["APPDATA", "LOCALAPPDATA"] {
            if let Some(d) = env_dir(var) {
                let _ = std::fs::remove_dir_all(d.join(NAME));
            }
        }
        crate::key::forget();
    }

    if let Some(dir) = dir().filter(|d| d.exists()) {
        if is_installed_copy() {
            // A running exe can't delete itself: a hidden shell does it once we've exited.
            let _ = std::process::Command::new("cmd")
                .raw_arg(format!(
                    "/d /c ping -n 3 127.0.0.1 >nul & rmdir /s /q \"{}\"",
                    dir.display()
                ))
                .creation_flags(CREATE_NO_WINDOW)
                .spawn();
        } else {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
    if !quiet {
        message(
            &format!("{NAME} has been removed."),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}
