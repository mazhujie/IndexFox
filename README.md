# IndexFox · 索引狐 (V1.0)

> **Windows x64 本地全文检索器（解压即用便携版）**  
> 对指定目录中的 `TXT`、`PDF`、`MD`、`DOCX` 及常见代码/配置文本进行纯本地内容检索；支持精确匹配、子串匹配、编辑距离拼写容错、按页自动 OCR 兜底、自动/轮询/手动增量索引、系统字体自定义与右侧文档高亮预览。

[Windows x64 便携测试包](https://github.com/mazhujie/IndexFox/releases/tag/v0.1.0-m4)

---

## 1. 设计文稿与交互原型导航

设计文稿与交互原型见 [`docs/`](docs/)。

| 文档 / 原型 | 路径 | 说明 |
|---|---|---|
| **技术设计方案** | [`docs/01_技术设计方案.md`](docs/01_技术设计方案.md) | 命名规范、21 项已确认需求、总体架构、SQLite FTS5 + `short_grams` 数据模型、分块与跨块一致性、安全与恢复设计 |
| **Codex 开发任务** | [`docs/02_Codex_开发任务.md`](docs/02_Codex_开发任务.md) | 交付规则、标准仓库目录结构、`M0`–`M4` 分阶段研发任务清单与验收门槛 |
| **验收测试矩阵** | [`docs/03_验收测试矩阵.md`](docs/03_验收测试矩阵.md) | `Q01`–`Q32` 最小验收测试用例集及性能基准记录规范 |
| **实现架构** | [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) | crate 边界、迁移 v1–v4、索引/查询/OCR 管线与 Windows 便携打包 |
| **查询语法** | [`docs/QUERY_SYNTAX.md`](docs/QUERY_SYNTAX.md) | AST、严格匹配、fuzzy 阈值与词典上限、排序和解释字段 |
| **基准报告** | [`docs/BENCHMARK.md`](docs/BENCHMARK.md) | 1 万与 10 万文件的性能和准确性验证 |
| **M4 Windows 验证** | [`scripts/test-m4-windows.ps1`](scripts/test-m4-windows.ps1) | 用隔离数据目录验证 ZIP 启动、OCR、容错与页码定位 |
| **UI 交互原型 (HTML)** | [`docs/prototype/index.html`](docs/prototype/index.html) | 零外部依赖的纯离线单文件交互原型，完整演示 `Q01`–`Q32` 检索交互、系统字体读取与实时换肤、多格式预览及异常保护 |

---

## 2. 仓库目录结构规范

```text
indexfox/
├── README.md                    # 项目总览与文档导航
├── docs/
│   ├── 01_技术设计方案.md        # V1.0 技术设计方案（21项确认需求）
│   ├── 02_Codex_开发任务.md      # M0–M4 分阶段开发任务清单
│   ├── 03_验收测试矩阵.md        # Q01–Q32 验收测试矩阵
│   ├── ARCHITECTURE.md          # 模块实现架构（M4 维护）
│   ├── QUERY_SYNTAX.md          # 查询语法、fuzzy 语义与上限
│   ├── BENCHMARK.md             # M4.6 基准 harness 与记录
│   └── prototype/
│       └── index.html           # V1.0 交互式桌面 UI 原型
├── crates/
│   ├── indexfox-core/           # 核心模型、配置（含字体/资源模式）、统一接口
│   ├── indexfox-source/         # 目录扫描、排除规则、扫描完整性与增量对账
│   ├── indexfox-extract/        # TXT/MD/DOCX/PDF/Tesseract OCR 解析器
│   ├── indexfox-index/          # SQLite WAL、FTS5 trigram、short_grams、fuzzy 词典、OCR 页缓存
│   ├── indexfox-query/          # 查询 AST 解析、严格核对、fuzzy 回退、排序
│   └── indexfox-cli/            # 索引管线、CLI 验证与基准测试工具
├── app/
│   ├── src/                     # React + TypeScript 搜索、来源与设置界面
│   └── src-tauri/               # Tauri 2 宿主、调度器、数据库迁移、桌面设置与字体
├── fixtures/                    # 合成测试样本（严禁存放个人隐私数据）
├── tests/                       # 集成与端到端回归测试
└── scripts/                     # 便携包组装、依赖校验与故障注入脚本
```

---

## 3. 当前进度（M4 功能和便携包已完成）

- ✅ M0–M2：6 个 Rust crate；SQLite FTS5 trigram + `short_grams` 短词/跨块检索；TXT/MD/源码、GBK/UTF-16、DOCX、PDFium PDF 提取；AST 查询与文件级布尔；Windows GUI 搜索、预览、历史、分页。
- ✅ M1.5–M1.7：单文件写入原子提交，失败保留旧索引并标 `stale`；`stats` 报告 `short_grams` 与 `page_count × page_size`；`index <ROOT> [--file <相对路径>]` 支持目录索引、单文件补索引和大小上限。
- ✅ M3：逐来源扫描策略与排除规则；notify 合并、轮询回查、SMB/WSL 断连保留旧索引；暂停/恢复/取消与 1/2/4 并发档；数据库迁移/回滚、缓存清理、重建；字体、托盘和开机启动设置。
- ✅ M4.1/M4.2/M4.5 OCR：PDF 逐页质量分类，仅空/稀疏/乱码页按 DPI 渲染 OCR；默认开启、`chi_sim+eng`、250 DPI、60 秒/页、并发 2、单页 4000 万像素；页面报告记录 `origin`/`from_cache`/`cache_key`/`chars`，失败页可单独重试。迁移 v4 增加 `ocr_page_cache`/`ocr_profile`；替换仍有失败页时保留旧的可检索 PDF，只补缓存并标 `stale`。
- ✅ M4.3/M4.4 fuzzy：迁移 v3 建立独立 `lexemes` 词典并回填旧 chunks；仅正向裸 term 做编辑距离回退，短语与 `NOT` 保持严格；严格命中在所有排序键下优先；结果附加 `strict`/`fuzzy`/`edit_distance`/`matched_lexeme`。
- ✅ Windows 便携 ZIP 约 480 MiB，随包携带 PDFium、Tesseract 5.5.4、中英文模型、Fixed WebView2、许可证和逐文件 SHA256 清单。Windows 11 非提权模式通过解压启动和主要功能验收。干净 Windows 与系统断网仍需验证。 [M4 开发日志](docs/02_Codex_开发任务.md)

详细证据见 [`docs/M0_技术验证报告.md`](docs/M0_技术验证报告.md)，实现边界见 [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)，语法见 [`docs/QUERY_SYNTAX.md`](docs/QUERY_SYNTAX.md)。

### 快速开始

```bash
cargo test --workspace

# 生成合成 fixtures（已提交，可重新生成）
python3 scripts/make_fixtures.py

# 索引并检索
cargo run -p indexfox-cli -- --db /tmp/indexfox.db index fixtures/text
cargo run -p indexfox-cli -- --db /tmp/indexfox.db search 内核驱动
cargo run -p indexfox-cli -- --db /tmp/indexfox.db search 'Linux -Windows'
cargo run -p indexfox-cli -- --db /tmp/indexfox.db search kernal --mode fuzzy
cargo run -p indexfox-cli -- --db /tmp/indexfox.db stats

# PDF OCR 默认开启；--no-ocr 关闭本次运行，--ocr 覆盖已保存的关闭状态。
# --tesseract/--lang 可指定本次可执行文件与模型组合。
export TESSERACT_PATH="$(command -v tesseract)"
cargo run -p indexfox-cli -- --db /tmp/indexfox.db index fixtures/pdf --lang chi_sim+eng
cargo run -p indexfox-cli -- --db /tmp/indexfox.db index fixtures/pdf --ocr
cargo run -p indexfox-cli -- --db /tmp/indexfox.db index fixtures/pdf --no-ocr

# 单文件补索引：只处理 ROOT 下的 RELATIVE_PATH，不做目录对账。
# 示例先用 0 MB 上限让目录内文件全部只留元数据，再只给一个文件调高上限；
# --force 绕过未变化跳过，但仍受本次大小上限约束。
cargo run -p indexfox-cli -- --db /tmp/indexfox.db \
  index fixtures/text --max-size-mb 0
cargo run -p indexfox-cli -- --db /tmp/indexfox.db \
  index fixtures/text --file kernel.md --max-size-mb 128 --force

# FTS5 trigram / 1–2 字符演示
cargo run -p indexfox-cli -- fts-demo
```

`--file` 接受 ROOT 内的相对文件路径，拒绝 `..` 和符号链接，并应用默认忽略目录和格式白名单。`--max-size-mb` 只应用于本次索引。超限文件只写元数据，文件名仍可检索。之后对同一目录扫描时，扫描使用该次的大小上限。

```bash
# PDF（Linux 验证用）
./scripts/fetch-pdfium.sh
export PDFIUM_LIB_PATH="$PWD/.deps/pdfium/lib/libpdfium.so"
cargo run -p indexfox-cli -- extract fixtures/pdf/sample.pdf
cargo run -p indexfox-cli -- pdf-render fixtures/pdf/sample.pdf --page 2 --out /tmp/page2.png
```

Windows 便携发布包（交叉编译，需 MinGW + Rust GNU target）：

```bash
# 静态 Tesseract 5.5.4：Leptonica/libpng/zlib 与 GCC 运行库全部静态链接。
./scripts/build-tesseract-windows.sh

# 获取并校验 PDFium、Fixed WebView2，并校验已构建的 Tesseract 目录清单。
./scripts/fetch-windows-runtime.sh

# 组装含固定版运行时的便携 ZIP。
./scripts/build-windows.sh
# 产物：target/IndexFox-windows-x64-portable.zip

# M4 实机冒烟使用隔离用户数据目录。
# powershell -ExecutionPolicy Bypass -File scripts\test-m4-windows.ps1 `
#   -Zip target\IndexFox-windows-x64-portable.zip
```

Tauri 壳：`cd app && npm install && npm run build`。完整发布包要求 Fixed WebView2 直接位于 `runtime/webview2`（该目录下即有 `msedgewebview2.exe`）；`SKIP_WEBVIEW2=1` 只用于依赖系统 Evergreen 的开发包。

## 4. 核心命名与运行约定

- **英文/中文名称**：`IndexFox` / `索引狐`
- **主程序与窗口标题**：`IndexFox.exe` / `IndexFox · 索引狐`
- **默认数据目录**：`%LOCALAPPDATA%\IndexFox\`（含 `data\index.db` 与 `config\settings.json`）
- **便携发行包名**：`IndexFox-windows-x64-portable.zip`
