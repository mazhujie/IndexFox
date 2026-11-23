use std::path::Path;

#[derive(serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileAction {
    Open,
    Reveal,
}

#[cfg(windows)]
pub fn perform(path: &Path, action: FileAction) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::System::Com::{
        CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
    };
    use windows_sys::Win32::UI::Shell::{
        ILFree, SHOpenFolderAndSelectItems, SHParseDisplayName, ShellExecuteW, SE_ERR_NOASSOC,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // Both Shell operations can call COM handlers. This worker owns its COM
    // initialization and releases it after the operation returns.
    let initialized = unsafe {
        CoInitializeEx(
            null(),
            (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
        )
    };
    if initialized < 0 {
        return Err(format!(
            "无法初始化 Windows 文件操作（{initialized:#010x}）。"
        ));
    }

    let result = match action {
        FileAction::Open => {
            let code = unsafe {
                ShellExecuteW(
                    null_mut(),
                    null(),
                    wide.as_ptr(),
                    null(),
                    null(),
                    SW_SHOWNORMAL,
                )
            } as isize;
            if code > 32 {
                Ok(())
            } else if code == SE_ERR_NOASSOC as isize {
                Err("此文件类型没有关联的默认应用。".into())
            } else {
                Err(format!("Windows 无法打开此文件（{code}）。"))
            }
        }
        FileAction::Reveal => {
            let mut item = null_mut();
            let parsed =
                unsafe { SHParseDisplayName(wide.as_ptr(), null_mut(), &mut item, 0, null_mut()) };
            if parsed < 0 || item.is_null() {
                Err(format!("资源管理器无法定位此文件（{parsed:#010x}）。"))
            } else {
                // A single absolute item identifier with no child array tells
                // Explorer to open its parent folder and select the item.
                let shown = unsafe { SHOpenFolderAndSelectItems(item, 0, null(), 0) };
                unsafe { ILFree(item) };
                if shown < 0 {
                    Err(format!("资源管理器无法显示此文件（{shown:#010x}）。"))
                } else {
                    Ok(())
                }
            }
        }
    };
    unsafe { CoUninitialize() };
    result
}

#[cfg(not(windows))]
pub fn perform(_path: &Path, _action: FileAction) -> Result<(), String> {
    Err("此文件操作适用于 Windows。".into())
}
