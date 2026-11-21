//! Desktop-only settings: close-to-tray, launch on startup, fonts and tray.
//!
//! The startup registry entry changes only when the user saves a new
//! `launchOnStartup` value.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError, RwLock};

use serde::{Deserialize, Serialize};
use tauri::menu::MenuBuilder;
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State};
use tempfile::NamedTempFile;

/// Tray exit requests the normal application shutdown and WAL checkpoint.
pub const EXIT_REQUESTED_EVENT: &str = "indexfox://desktop-exit-requested";

pub const PREVIEW_FONT_SIZE_MIN: u16 = 8;
pub const PREVIEW_FONT_SIZE_MAX: u16 = 72;
pub const DEFAULT_PREVIEW_FONT_SIZE: u16 = 14;

const TRAY_ID: &str = "indexfox-tray";
const TRAY_SHOW: &str = "indexfox-tray-show";
const TRAY_EXIT: &str = "indexfox-tray-exit";

/// Persisted desktop settings. Empty font names mean "use the front-end default
/// stack"; unknown/absent fields keep defaults so old JSON files still load.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DesktopSettings {
    #[serde(alias = "close_to_tray")]
    pub close_to_tray: bool,
    #[serde(alias = "launch_on_startup")]
    pub launch_on_startup: bool,
    #[serde(alias = "ui_font")]
    pub ui_font: String,
    #[serde(alias = "preview_font")]
    pub preview_font: String,
    #[serde(alias = "preview_font_size")]
    pub preview_font_size: u16,
}

impl Default for DesktopSettings {
    fn default() -> Self {
        Self {
            close_to_tray: false,
            launch_on_startup: false,
            ui_font: String::new(),
            preview_font: String::new(),
            preview_font_size: DEFAULT_PREVIEW_FONT_SIZE,
        }
    }
}

impl DesktopSettings {
    fn sanitized(mut self) -> Self {
        self.ui_font = self.ui_font.trim().to_string();
        self.preview_font = self.preview_font.trim().to_string();
        self.preview_font_size = self
            .preview_font_size
            .clamp(PREVIEW_FONT_SIZE_MIN, PREVIEW_FONT_SIZE_MAX);
        self
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub struct DesktopState {
    settings_path: PathBuf,
    settings: RwLock<DesktopSettings>,
    tray: Mutex<Option<TrayIcon>>,
    /// Serializes read-modify-write of the settings file and registry.
    write_lock: Mutex<()>,
}

impl DesktopState {
    /// Read the JSON file if present, otherwise defaults. A corrupt/partial file
    /// falls back to defaults instead of blocking startup; it is not rewritten
    /// until the user actually saves.
    pub fn load(settings_path: PathBuf) -> Self {
        let settings = fs::read(&settings_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<DesktopSettings>(&bytes).ok())
            .unwrap_or_default()
            .sanitized();
        Self {
            settings_path,
            settings: RwLock::new(settings),
            tray: Mutex::new(None),
            write_lock: Mutex::new(()),
        }
    }

    pub fn settings(&self) -> DesktopSettings {
        self.settings
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn tray_installed(&self) -> bool {
        lock(&self.tray).is_some()
    }

    fn remove_tray(&self) {
        lock(&self.tray).take();
    }

    fn persist(&self, settings: &DesktopSettings) -> Result<(), String> {
        write_settings(&self.settings_path, settings)
    }
}

#[tauri::command]
pub fn get_desktop_settings(state: State<'_, DesktopState>) -> DesktopSettings {
    state.settings()
}

/// The only path that can touch the current user's Run key: it is called from
/// this command only when `launchOnStartup` actually changes.
#[tauri::command]
pub fn set_desktop_settings(
    settings: DesktopSettings,
    app: AppHandle,
    state: State<'_, DesktopState>,
) -> Result<DesktopSettings, String> {
    let _guard = lock(&state.write_lock);
    let previous = state.settings();
    let next = settings.sanitized();

    if previous == next {
        if next.close_to_tray {
            ensure_tray(&app, &state)?;
        }
        return Ok(next);
    }

    let mut installed_tray = false;
    if next.close_to_tray && !state.tray_installed() {
        ensure_tray(&app, &state)?;
        installed_tray = true;
    }

    let startup_changed = previous.launch_on_startup != next.launch_on_startup;
    if let Err(error) = apply_startup_if_changed(
        previous.launch_on_startup,
        next.launch_on_startup,
        apply_launch_on_startup,
    ) {
        if installed_tray {
            state.remove_tray();
        }
        return Err(error);
    }

    if let Err(error) = state.persist(&next) {
        if startup_changed {
            let _ = apply_launch_on_startup(previous.launch_on_startup);
        }
        if installed_tray {
            state.remove_tray();
        }
        return Err(error);
    }

    *state
        .settings
        .write()
        .unwrap_or_else(PoisonError::into_inner) = next.clone();
    if !next.close_to_tray {
        state.remove_tray();
    }
    Ok(next)
}

#[tauri::command]
pub fn system_fonts() -> Vec<String> {
    platform_system_fonts()
}

/// Install the tray when configured. Tray failure must not prevent startup;
/// [`close_to_tray`] refuses to hide without a live tray, so a failed tray
/// degrades to "close exits" rather than an unreachable window.
pub fn setup(app: &mut tauri::App) -> tauri::Result<()> {
    let state = app.state::<DesktopState>();
    if state.settings().close_to_tray {
        if let Err(error) = ensure_tray(app.handle(), state.inner()) {
            eprintln!("indexfox: cannot install tray icon: {error}");
        }
    }
    Ok(())
}

/// Returns `true` only when the settings ask for tray residency *and* the tray
/// actually exists; the caller should then `prevent_close()`. A failed
/// `hide()` returns `false` so the window is never lost.
pub fn close_to_tray(window: &tauri::Window) -> bool {
    let Some(state) = window.app_handle().try_state::<DesktopState>() else {
        return false;
    };
    if !state.settings().close_to_tray || !state.tray_installed() {
        return false;
    }
    window.hide().is_ok()
}

fn ensure_tray(app: &AppHandle, state: &DesktopState) -> Result<(), String> {
    if state.tray_installed() {
        return Ok(());
    }
    let tray = build_tray(app).map_err(|error| format!("cannot create tray icon: {error}"))?;
    *lock(&state.tray) = Some(tray);
    Ok(())
}

fn build_tray(app: &AppHandle) -> tauri::Result<TrayIcon> {
    let menu = MenuBuilder::new(app)
        .text(TRAY_SHOW, "显示主窗口")
        .separator()
        .text(TRAY_EXIT, "退出 IndexFox")
        .build()?;

    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .menu(&menu)
        .tooltip("IndexFox · 索引狐")
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            TRAY_SHOW => show_main_window(app),
            TRAY_EXIT => {
                let _ = app.emit(EXIT_REQUESTED_EVENT, ());
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        });

    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)
}

fn show_main_window(app: &AppHandle) {
    let window = app
        .get_webview_window("main")
        .or_else(|| app.webview_windows().into_values().next());
    if let Some(window) = window {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn apply_startup_if_changed<F>(previous: bool, next: bool, mut apply: F) -> Result<(), String>
where
    F: FnMut(bool) -> Result<(), String>,
{
    if previous == next {
        return Ok(());
    }
    apply(next)
}

/// Atomic replace: write and sync a sibling temp file, then rename over the
/// target. `tempfile::persist` uses `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` on
/// Windows, so an interrupted save leaves the previous file intact.
fn write_settings(path: &Path, settings: &DesktopSettings) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| {
        format!(
            "cannot create settings directory {}: {error}",
            parent.display()
        )
    })?;

    let mut temp = NamedTempFile::new_in(parent)
        .map_err(|error| format!("cannot create temporary settings file: {error}"))?;
    {
        let file = temp.as_file_mut();
        serde_json::to_writer_pretty(&mut *file, settings)
            .map_err(|error| format!("cannot serialize desktop settings: {error}"))?;
        file.write_all(b"\n")
            .map_err(|error| format!("cannot write desktop settings: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("cannot sync desktop settings: {error}"))?;
    }
    temp.persist(path).map_err(|error| {
        format!(
            "cannot replace desktop settings {}: {}",
            path.display(),
            error.error
        )
    })?;
    Ok(())
}

#[cfg(windows)]
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
#[cfg(windows)]
const RUN_VALUE: &str = "IndexFox";

fn startup_command_line(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

#[cfg(windows)]
fn apply_launch_on_startup(enabled: bool) -> Result<(), String> {
    let windows = std::env::var_os("SystemRoot").ok_or("cannot locate the Windows directory")?;
    let reg = PathBuf::from(windows).join("System32").join("reg.exe");
    let mut command = std::process::Command::new(reg);
    if enabled {
        let exe = std::env::current_exe()
            .map_err(|error| format!("cannot locate IndexFox executable: {error}"))?;
        let value = startup_command_line(&exe);
        command.args([
            "add", RUN_KEY, "/v", RUN_VALUE, "/t", "REG_SZ", "/d", &value, "/f",
        ]);
    } else {
        command.args(["delete", RUN_KEY, "/v", RUN_VALUE, "/f"]);
    }

    let output = command
        .output()
        .map_err(|error| format!("cannot run reg.exe: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = stderr.trim();
    let detail = if detail.is_empty() {
        stdout.trim()
    } else {
        detail
    };
    Err(format!(
        "reg.exe {} failed with {}{}",
        if enabled { "add" } else { "delete" },
        output.status,
        if detail.is_empty() {
            String::new()
        } else {
            format!(": {detail}")
        }
    ))
}

#[cfg(not(windows))]
fn apply_launch_on_startup(enabled: bool) -> Result<(), String> {
    if enabled {
        Err("launch on startup is only supported on Windows".to_string())
    } else {
        Ok(())
    }
}

fn face_name(face: &[u16]) -> Option<String> {
    let len = face
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(face.len());
    if len == 0 {
        return None;
    }
    let name = String::from_utf16(&face[..len]).ok()?;
    let name = name.trim();
    if name.is_empty() || name.starts_with('@') {
        return None;
    }
    Some(name.to_string())
}

#[derive(Default)]
struct FontEnumeration {
    seen: HashSet<String>,
    fonts: Vec<String>,
}

impl FontEnumeration {
    fn push(&mut self, face: &[u16]) {
        let Some(name) = face_name(face) else {
            return;
        };
        if self.seen.insert(name.to_lowercase()) {
            self.fonts.push(name);
        }
    }

    fn finish(mut self) -> Vec<String> {
        self.fonts
            .sort_by(|left, right| left.to_lowercase().cmp(&right.to_lowercase()));
        self.fonts
    }
}

#[cfg(windows)]
fn platform_system_fonts() -> Vec<String> {
    use windows_sys::Win32::Foundation::LPARAM;
    use windows_sys::Win32::Graphics::Gdi::{
        EnumFontFamiliesExW, GetDC, ReleaseDC, DEFAULT_CHARSET, LOGFONTW, TEXTMETRICW,
    };

    unsafe extern "system" fn collect(
        logfont: *const LOGFONTW,
        _metric: *const TEXTMETRICW,
        _font_type: u32,
        lparam: LPARAM,
    ) -> i32 {
        if lparam != 0 && !logfont.is_null() {
            let enumeration = &mut *(lparam as *mut FontEnumeration);
            enumeration.push(&(*logfont).lfFaceName);
        }
        1
    }

    let device = unsafe { GetDC(std::ptr::null_mut()) };
    if device.is_null() {
        return Vec::new();
    }

    let mut logfont = LOGFONTW::default();
    logfont.lfCharSet = DEFAULT_CHARSET;
    let mut enumeration = FontEnumeration::default();
    unsafe {
        EnumFontFamiliesExW(
            device,
            &logfont,
            Some(collect),
            &mut enumeration as *mut _ as LPARAM,
            0,
        );
        ReleaseDC(std::ptr::null_mut(), device);
    }
    enumeration.finish()
}

#[cfg(not(windows))]
fn platform_system_fonts() -> Vec<String> {
    // No portable installed-font API is available here; the front end falls
    // back to its own dynamic/default font stack when the list is empty.
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_with_size(size: u16) -> DesktopSettings {
        DesktopSettings {
            preview_font_size: size,
            ..DesktopSettings::default()
        }
    }

    #[test]
    fn defaults_close_and_autostart_are_off() {
        let settings = DesktopSettings::default();
        assert!(!settings.close_to_tray);
        assert!(!settings.launch_on_startup);
        assert_eq!(settings.preview_font_size, DEFAULT_PREVIEW_FONT_SIZE);
        assert!(settings.ui_font.is_empty());
        assert!(settings.preview_font.is_empty());
    }

    #[test]
    fn missing_file_loads_defaults_without_creating_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("desktop.json");
        let state = DesktopState::load(path.clone());
        assert_eq!(state.settings(), DesktopSettings::default());
        assert!(!path.exists());
    }

    #[test]
    fn corrupt_file_loads_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("desktop.json");
        fs::write(&path, b"{ not json").unwrap();
        assert_eq!(
            DesktopState::load(path).settings(),
            DesktopSettings::default()
        );
    }

    #[test]
    fn partial_json_keeps_defaults_and_clamps_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("desktop.json");
        fs::write(
            &path,
            br#"{"closeToTray":true,"uiFont":"  Inter  ","previewFontSize":999,"unknown":1}"#,
        )
        .unwrap();

        let settings = DesktopState::load(path).settings();
        assert!(settings.close_to_tray);
        assert!(!settings.launch_on_startup);
        assert_eq!(settings.ui_font, "Inter");
        assert_eq!(settings.preview_font, "");
        assert_eq!(settings.preview_font_size, PREVIEW_FONT_SIZE_MAX);
    }

    #[test]
    fn sanitize_clamps_preview_size() {
        assert_eq!(
            settings_with_size(0).sanitized().preview_font_size,
            PREVIEW_FONT_SIZE_MIN
        );
        assert_eq!(
            settings_with_size(PREVIEW_FONT_SIZE_MAX + 1)
                .sanitized()
                .preview_font_size,
            PREVIEW_FONT_SIZE_MAX
        );
        assert_eq!(
            settings_with_size(DEFAULT_PREVIEW_FONT_SIZE)
                .sanitized()
                .preview_font_size,
            DEFAULT_PREVIEW_FONT_SIZE
        );
    }

    #[test]
    fn save_round_trips_atomically_and_replaces_previous_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("desktop.json");
        let state = DesktopState::load(path.clone());
        let expected = DesktopSettings {
            close_to_tray: true,
            launch_on_startup: false,
            ui_font: "Segoe UI".into(),
            preview_font: "Consolas".into(),
            preview_font_size: 18,
        };
        state.persist(&expected).unwrap();
        assert_eq!(DesktopState::load(path.clone()).settings(), expected);

        let updated = DesktopSettings {
            ui_font: String::new(),
            preview_font_size: 12,
            ..expected
        };
        state.persist(&updated).unwrap();
        assert_eq!(DesktopState::load(path.clone()).settings(), updated);

        // Only the final file remains; the temp sibling was renamed over it.
        let entries: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("desktop.json")]);
    }

    #[test]
    fn json_uses_camel_case_keys() {
        let value = serde_json::to_value(DesktopSettings::default()).unwrap();
        for key in [
            "closeToTray",
            "launchOnStartup",
            "uiFont",
            "previewFont",
            "previewFontSize",
        ] {
            assert!(value.get(key).is_some(), "missing {key}");
        }
    }

    #[test]
    fn startup_is_applied_only_on_explicit_change() {
        let calls = std::cell::RefCell::new(Vec::new());
        let mut apply = |value: bool| {
            calls.borrow_mut().push(value);
            Ok(())
        };
        apply_startup_if_changed(false, false, &mut apply).unwrap();
        apply_startup_if_changed(true, true, &mut apply).unwrap();
        assert!(calls.borrow().is_empty());

        apply_startup_if_changed(false, true, &mut apply).unwrap();
        apply_startup_if_changed(true, false, &mut apply).unwrap();
        assert_eq!(*calls.borrow(), vec![true, false]);
    }

    #[test]
    fn startup_command_quotes_the_executable_path() {
        let path = Path::new(r"C:\Program Files\IndexFox\IndexFox.exe");
        assert_eq!(
            startup_command_line(path),
            r#""C:\Program Files\IndexFox\IndexFox.exe""#
        );
    }

    #[test]
    fn font_enumeration_skips_vertical_and_duplicate_families() {
        let mut enumeration = FontEnumeration::default();
        let mut face = [0u16; 32];
        for (slot, unit) in face.iter_mut().zip("Segoe UI".encode_utf16()) {
            *slot = unit;
        }
        enumeration.push(&face);

        let mut duplicate = [0u16; 32];
        for (slot, unit) in duplicate.iter_mut().zip("segoe ui".encode_utf16()) {
            *slot = unit;
        }
        enumeration.push(&duplicate);

        let mut vertical = [0u16; 32];
        for (slot, unit) in vertical.iter_mut().zip("@Segoe UI".encode_utf16()) {
            *slot = unit;
        }
        enumeration.push(&vertical);

        assert_eq!(enumeration.finish(), vec!["Segoe UI".to_string()]);
    }

    #[cfg(not(windows))]
    #[test]
    fn font_list_is_empty_off_windows() {
        assert!(system_fonts().is_empty());
    }
}
