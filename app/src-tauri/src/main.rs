#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    #[cfg(windows)]
    use_portable_webview2();
    indexfox_app_lib::run()
}

#[cfg(windows)]
fn use_portable_webview2() {
    const RUNTIME: &str = "WEBVIEW2_BROWSER_EXECUTABLE_FOLDER";
    if std::env::var_os(RUNTIME).is_some() {
        return;
    }
    if let Some(folder) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("runtime").join("webview2")))
        .filter(|folder| folder.join("msedgewebview2.exe").is_file())
    {
        // Configure this process before Tauri starts any threads.
        std::env::set_var(RUNTIME, folder);
    }
}
