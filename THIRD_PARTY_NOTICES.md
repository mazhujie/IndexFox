# Third-Party Notices

> 状态：M4.7 打包清单已固定版本与 SHA256；Tesseract 已改为静态链接宽松许可构建，旧 UB Mannheim 安装器的 GPL/LGPL DLL 闭包已移除。**发布前仍需确认项目级 LICENSE 与 WebView2 EULA 条款传递**；包内 `licenses/` 随 ZIP 附带对应许可证、来源与构建证明。

## Rust crates（直接依赖）

| 组件 | 版本（Cargo 解析） | 许可证 |
|---|---|---|
| rusqlite | 0.32.1 | MIT |
| libsqlite3-sys / SQLite | 0.30.1 / 3.46.0 | MIT / Public Domain |
| encoding_rs | 0.8.44 | Apache-2.0 OR MIT |
| zip | 2.6.1 | MIT |
| quick-xml | 0.36.2 | MIT OR Apache-2.0 |
| walkdir | 2.5.0 | Unlicense OR MIT |
| clap | 4.6.9 | MIT OR Apache-2.0 |
| thiserror | 2.x | MIT OR Apache-2.0 |
| serde / serde_json | 1.x | MIT OR Apache-2.0 |
| chrono | 0.4.45 | MIT OR Apache-2.0 |
| anyhow | 1.0.104 | MIT OR Apache-2.0 |
| pdfium-render | 0.8.37 | MIT OR Apache-2.0 |
| tempfile | 3.27.0 | MIT OR Apache-2.0 |
| image（PNG 编码） | 0.25.10 | MIT OR Apache-2.0 |
| notify | 8.2.2 | CC0-1.0 |
| windows-sys | 0.61.2 | MIT OR Apache-2.0 |
| tray-icon（Tauri 托盘功能） | 0.25.1 | MIT OR Apache-2.0 |

传递依赖的许可证以 `Cargo.lock` 与 <https://crates.io> 元数据为准；发布前应生成完整清单（例如 `cargo about`/`cargo deny`）。

## PDFium（随 Windows 便携包分发）

- 来源：`bblanchon/pdfium-binaries`，Chromium build `160.0.8165.0`。
- Windows x64 包：`https://github.com/bblanchon/pdfium-binaries/releases/download/chromium%2F8165/pdfium-win-x64.tgz`
- 压缩包 SHA256：`ca7338ae7779b8b1b82f4580305c4d762d214f8df1d54b47647fe3cc75989ded`
- `pdfium.dll` SHA256：`31d249b8535dd5f351ae7f4b714dc4bb1d76b6069b321a309bc99271dd65f566`
- Linux 验证包 SHA256：`ba7865e16c0271daab75256790be06c26272190afe6e2a70eae0bb39bb99089c`（`libpdfium.so`：`63b70290e3e88eff6b236a4c87cb58461b4fe9fae56217314eeda31aff741479`）。
- 打包脚本 MIT（Copyright 2014-2025 Benoit Blanchon）。PDFium 本体 BSD-3-Clause；完整第三方清单取自压缩包 `licenses/`，随包放入 `licenses/PDFium-third-party/`。
- 包内文件：`licenses/PDFium-binaries-MIT.txt`、`licenses/PDFium-BSD-3-Clause.txt`、`licenses/PDFium-third-party/`。

## Tesseract（Windows x64，静态构建，随包分发）

打包使用 `scripts/build-tesseract-windows.sh` 产出的静态构建目录 `.deps/windows/tesseract-minimal/`。`scripts/fetch-windows-runtime.sh` 只校验该目录的 `SHA256SUMS.txt`，不再下载或解包安装器；旧 UB Mannheim NSIS 安装器及其 DLL 闭包已从打包流程移除。

- 版本：Tesseract 5.5.4，源码为官方 GitHub tag 压缩包
  `https://codeload.github.com/tesseract-ocr/tesseract/tar.gz/refs/tags/5.5.4`
  （SHA256 `a8dbf8b8c8ee427ff6ad1d99116502feaf59440af1adb893caf60a6576826cf4`）。
- 静态依赖（不随包分发 DLL）：
  - Leptonica 1.87.0（BSD-2-Clause），仅启用 PNG 输入；
  - libpng 1.6.59（libpng-2.0）；
  - zlib 1.4（zlib license）；
  - GCC 13 MinGW-w64 静态运行时（GPL-3.0 WITH GCC Runtime Library Exception 3.1，允许专有链接）；MinGW-w64 CRT 许可见 `licenses/Tesseract-dependencies/MinGW-w64-COPYRIGHT.txt`。
- `tesseract.exe` 为静态链接；实际 SHA256 与 PE 导入表见 `licenses/Tesseract-BUILD-INFO.txt`（导入仅 `KERNEL32.dll`、`msvcrt.dll`，无需额外 DLL）。
- 模型来自 `tesseract-ocr/tessdata_fast` commit `87416418657359cb625c412a48b6e1d6d41c29bd`：
  - `eng.traineddata` SHA256 `7d4322bd2a7749724879683fc3912cb542f19906c83bcc1a52132556427170b2`
  - `chi_sim.traineddata` SHA256 `a5fcb6f0db1e1d6d8522f39db4e848f05984669172e584e8d76b6b3141e1f730`
  - 模型 Apache-2.0：`licenses/tessdata-Apache-2.0.txt`。
- 构建证明：`licenses/Tesseract-BUILD-INFO.txt`（版本、编译器、链接参数、PE 导入表、EXE SHA256）与 `licenses/Tesseract-SHA256SUMS.txt`（构建目录逐文件哈希）。
- 包内许可证：`licenses/Tesseract-Apache-2.0.txt`、`licenses/Tesseract-AUTHORS.txt`、`licenses/Tesseract-dependencies/`（Leptonica、libpng、zlib、GCC 运行时例外/GPL-3.0、MinGW-w64）。
- 不再随包分发 `libtesseract-5.dll`，也不包含 `liblzo2`、`libjbig`、`libidn2`、`libunistring`、`libiconv`、`libintl` 等 GPL/LGPL DLL。

## Microsoft Edge WebView2（Fixed Version，随包分发）

- 来源（Microsoft 官方下载页 Fix Version 选项生成）：`https://msedge.sf.dl.delivery.mp.microsoft.com/filestreamingservice/files/ecc2761e-1380-46ee-9552-995b039e4e47/Microsoft.WebView2.FixedVersionRuntime.158.0.4370.33.x64.cab`
- CAB SHA256：`835da387df67797f710881f553bb6934d98f24d08e370456f48d38ed4e072b62`
- 发行版：`158.0.4370.33` x64，解包约 946 MB；完整 CAB 内容**平铺**放在 ZIP 的 `runtime/webview2/`，`msedgewebview2.exe` 直接位于该目录（应用启动代码按此路径探测）。
- 许可：Microsoft Edge WebView2 Runtime (Fixed Version) EULA，随包 `licenses/WebView2-Fixed-LICENSE.txt`（取自 `https://developer.microsoft.com/microsoft-edge/api/eula/webview2?locale=en-us&fixed=true`）。
- 微软分发条款要求（EULA 第 2、8 节）：应用须增加实质功能、向最终用户传递同等保护条款、只从 Microsoft 官方渠道取得代码、不得修改/移除 Microsoft 及第三方声明、不得暗示微软背书；未禁用 SmartScreen 时必须向用户告知。便携包 README 与 `licenses/WebView2-third-party-NOTICE.txt` 已包含所需 SmartScreen 通知。
- 第三方声明由未修改的运行时自带：`runtime/webview2/show_third_party_software_licenses.bat`（`msedgewebview2.exe --embedded-browser-webview=show-credits`）。不得删除或修改。
- **不安装、不写注册表、不修改系统配置。** 应用在启动时探测 `runtime/webview2/msedgewebview2.exe`，并为本进程设置 `WEBVIEW2_BROWSER_EXECUTABLE_FOLDER`。
- 实测目标为 **Windows 11 x64**；Windows 10 未纳入本轮实测。Windows 10（非 11）自 Fixed Version 120 起，未打包 Win32 应用需要 AppContainer SID 读取权限；README 给出用户自行执行的 `icacls` 命令，未自动修改系统。
- Fixed Version 不能从网络位置或 UNC 路径运行，必须放在本地磁盘目录；官方分发说明：<https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/distribution>。
- Fixed Version 不自动更新；随包发布新版本以获取安全修复。
- 代码签名尚未配置，SmartScreen 可能对未签名 EXE 提示。

## 字体

- 应用读取系统已安装字体，不随包分发字体文件。界面默认使用系统字体栈；用户自定义字体不回退到未授权字体内置文件。

## 项目自身许可证

仓库尚未选择/添加项目级 LICENSE 文件；发布前需确认。此前 `Cargo.toml` 中的占位许可证声明已移除，避免与实际情况不一致。
