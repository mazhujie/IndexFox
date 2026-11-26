# IndexFox 模块实现架构

> 对应阶段为 M0 技术验证、M1 检索核心、M2 桌面体验、M3 索引调度/可靠性/设置，以及 M4 OCR、拼写容错与便携打包。

## 1. Crate 边界

| Crate | 责任 | 已实现 |
|---|---|---|
| `indexfox-core` | 命名常量、配置、`ExtractedDocument`/`Section`/`SourceLocator`、ASCII 大小写规范化 | 完整模型；`SourceScanConfig`、`ResourceMode`、`OcrConfig` 默认值 |
| `indexfox-source` | 目录枚举、单文件检查、扩展名白名单、排除规则、大小上限、符号链接/重解析点不跟随、扫描完整性标记 | 单线程扫描器 + `scan_file` |
| `indexfox-extract` | TXT/MD/源码编码识别与分块、DOCX OOXML、PDFium PDF、Tesseract OCR 适配 | 真实实现；页面质量分类、OCR 缓存键/失败页/来源报告，PDF/OCR 依赖外部运行库 |
| `indexfox-index` | SQLite migration v1→v4、单文件原子替换、FTS5 trigram、`short_grams`、fuzzy 词典、OCR 页缓存、索引任务与扫描 generation | 单写多读、WAL；v3 回填词典，v4 持久化 OCR 配置/页缓存 |
| `indexfox-query` | AST 解析、文件级布尔求值、严格核对、fuzzy 回退、跨块核对、排序、片段生成 | exact/substring/fuzzy；严格命中优先，结果带解释字段 |
| `indexfox-cli` | 索引管线与验证 CLI | `index [--file/--ocr/--no-ocr]/search/stats/extract/ocr/pdf-render/fts-demo` |
| `app/src-tauri` | Tauri 2 命令、调度器、数据库维护和 Windows 宿主 | 搜索、扫描、缓存预览、PDF 原页、历史、notify/轮询调度、迁移、桌面/OCR 设置、Fixed WebView2 查找 |
| `app/src` | React + TypeScript 界面 | 搜索、来源、历史、调度状态和设置（含 OCR/fuzzy 结果提示） |

CLI 和桌面调度器复用同一套扫描和写入逻辑。

## 2. 来源配置与调度

来源的启用状态和扫描策略存储在 `sources`。其余字段通过 `SourceScanConfig` 从 `config_json`（camelCase）解析。

| 配置 | 行为 |
|---|---|
| `scanMode` | `auto`（默认）、`poll`、`manual`。本地 `auto` 注册 notify；非本地 `auto` 只轮询；`manual` 只在手动扫描/重建时执行。 |
| `recursive` | 默认 `true`；`false` 时扫描深度为 1，只处理根目录直接子文件。 |
| `extensions` | 大小写不敏感的扩展名白名单，默认覆盖文档、源码、配置和日志文本。 |
| `excludedDirs` | 目录名精确匹配；含 `*`/`?` 的条目按简单通配符匹配目录名。默认排除 `.git`、`node_modules`、`target` 等。 |
| `excludeGlobs` | 不含分隔符时匹配文件/目录名；含 `/` 或 `\` 时匹配相对来源根的规范化路径。仅支持 `*`/`?`，无转义和字符类。 |
| `maxFileSizeBytes` | 单文件索引上限默认 300 MB。超限文件只写元数据，状态为 `skipped_size`。PDF 原页预览使用全局默认上限 300 MB。 |
| `pollIntervalSecs` | 默认 60 秒；调度器夹取到 5–86400 秒，`set_source_config` 也按此范围校验。 |
| `enabled` | 停用来源不再自动排队，也不能手动扫描。 |

排除规则按来源根决定大小写：Windows/SMB 对 ASCII 大小写不敏感，WSL 与 POSIX 路径保留大小写。符号链接、目录 junction 和其他重解析点一律不跟随。

调度器只有一个协调线程，事件、轮询和队列都汇聚到 SQLite 索引任务：

- 本地 `auto` 的 notify 事件不立即扫描，而是为来源记录时间戳；同一来源静默 750 ms 后才排队。即使没有事件，也按 `pollIntervalSecs` 周期回查，补偿漏事件。
- SMB/WSL/可移动来源不注册 notify，只按轮询周期扫描；离线来源保留在计划中，按间隔重试。
- watcher 后端失败后每 60 秒重试；来源离线或停用时卸载 watcher。
- 暂停会停止排队和领取新任务；运行中的任务在检查点等待恢复。取消排队任务直接删除任务行；取消运行中任务只发出信号，管线保留旧索引并删除任务行。
- 自动来源失败后按轮询间隔重试；`manual` 来源不自动重试。进程启动时，遗留的 `running` 扫描重新排队；已尝试 3 次的任务标为失败，并将 `scanning` 状态恢复为 `unknown`。
- 并发上限按来源任务计：节能 1、均衡 2、高速 4，默认均衡；同一来源不会同时运行两个扫描任务。
- 来源状态为 `online`、`offline`、`scanning`、`failed`、`unknown`，断连只改状态，不删除文件行。

## 3. 写入路径与 generation

```text
调度器到期 -> index_registered_source(source_id)
  -> begin_source_scan()        // scan_generation += 1, online_state = 'scanning'
  -> scan_root_controlled()     // 取消/暂停检查，枚举快照
  -> 每个文件：大小/扩展名/排除规则/符号链接检查
       -> 读取前后各取 size+mtime，变化则重试一次
       -> extract_path_with_ocr_settings(...) // 可选 PageOcrCache
       -> PDF: IndexDb::store_pdf_index()     // 正文/页缓存/状态同一事务
          其他: IndexDb::store_file()         // 单文件事务
  -> finish_source_scan()       // 仅当前 generation 可提交
       标记 seen_generation
       删除本代未见的文件及其任务
       online_state = 'online', 记录 last_success_scan_at
  -> 出错 -> abort_source_scan() // offline / failed / unknown，不动文件行
```

- 只有完整（`complete == true`）且 generation 仍为当前代的扫描才能对账删除。取消、离线、根目录在扫描期间变化或枚举不完整时，保留未对账的旧记录和本次已提交的单文件版本。
- 取消运行中任务时，管线在下一个检查点返回并删除任务行。应用退出时会保留未完成的任务，供下次启动恢复。
- 单个文件写入失败时保留上次成功提交的元数据与索引。有旧索引的文件标为 `stale`，首次解析失败标为 `failed`；`SQLITE_FULL` 导致的事务回滚同样保留旧版本。
- PDF 替换仍有 OCR 失败页时保留旧的可检索正文，只写入本轮成功/命中缓存的页并把文件标为 `stale`；下次扫描只重试失败页（见 §4、§7）。
- 离线来源不删除任何记录；恢复后必须有一次完整扫描才允许对账。
- 重命名或移动时按新路径重新索引，成功对账后移除旧路径记录。
- CLI 单文件入口 `index <ROOT> --file <RELATIVE_PATH>` 仍只调用 `scan_file`，不遍历目录、不对账，其他文件记录保持不变；`--force` 只绕过未变化跳过，不放宽大小上限。

## 4. 数据模型

Migration v1 位于 `crates/indexfox-index/src/migrations.rs`，包含
`sources/files/chunks/chunks_fts/short_grams/search_history/index_jobs/app_meta`。
当前 `SCHEMA_VERSION = 5`。所有升级在 `BEGIN IMMEDIATE` 中串行执行；高于支持版本直接拒绝打开，等于当前版本不写库。V5 按 `short_grams.chunk_id` 建立索引，加速删除文本块时的级联清理。

- v2：`sources.scan_generation` 每次扫描开始原子递增，旧 generation 不能再提交；`files.seen_generation` 记录文件最后一次被完整扫描看到的代次，用于对账。不重建表，也不触碰已有 FTS5 external-content 行和 `short_grams`。
- v3：新增独立 fuzzy 词典 `lexemes`（ASCII 小写、Unicode 标量长度、`word`/`cjk`）、`lexeme_grams`（gram → lexeme）和 `lexeme_occurrences`（lexeme → chunk 字节区间）。词典跨文件共享；chunk 删除/替换级联 occurrence，`lexeme_occurrences_ad` 触发器与 `prune_orphan_lexemes()` 清理无引用的词。迁移在同一事务内用 `chunks.display_text` 回填 v1/v2 已提交的 chunk，已有 FTS/短词行不变。
- v4：新增 `files.ocr_profile` 和 `ocr_page_cache(file_id, cache_key, text)`（随文件删除级联）。`ocr_page_cache` 只存成功 OCR 页文本与稳定键；`ocr_profile` 仅在本轮全部页成功后写入，用于未变化文件跳过重新提取。
- `store_pdf_index()` 在同一事务发布正文、页缓存和状态：本轮仍有失败页且旧文件为 `indexed`/`stale` 时，不重写旧 `chunks`，保留旧的可检索 PDF，只写入本轮成功页与命中缓存的页、清空 `ocr_profile` 并标 `stale`；没有旧可检索版本时才写入部分正文。失败页不写缓存，下次扫描命中成功页缓存、只重试失败页。

其余模型：

- `chunks_fts` 是 external-content FTS5，`tokenize='trigram'`；插入/删除/更新由 trigger 同步，`verify_fts()` 运行 integrity-check。
- `chunks.text` 存规范化文本（仅 ASCII 小写），`display_text` 存原文；两者字节长度一致，匹配偏移可直接用于高亮。词典偏移同样使用 `display_text` 字节。
- `short_grams` 存每个 chunk 去重后的单字和双字 gram（忽略空白），供 1–2 字符查询和跨块召回兜底。词典写入与 chunk 替换在同一事务，失败不会留下孤立词。
- 体积诊断：`IndexDb::stats()` 以 `PRAGMA page_count × page_size` 报告逻辑库大小（`db_size_bytes`/`db_page_count`/`db_page_size`），并返回 `short_grams`、`lexemes`、`lexeme_grams`、`lexeme_occurrences` 计数。范围包括 schema、全部表、FTS5、`short_grams`、词典、OCR 页缓存、freelist 以及已提交到 WAL 但尚未 checkpoint 的页；不包括 WAL 文件头/帧与 SHM 开销、未提交 WAL 帧和页缓存。
- `source_locator_json` 保存 `Text`（闭区间行号、解码后字节偏移）、`PdfPage`（1 基页码）、`DocxParagraph`、`DocxTableCell`。查询用缓存正文修正旧库中多计的末尾换行。
- 搜索历史按表达式、模式和过滤条件去重，写入时在同一事务保留最近 100 条。
- `index_jobs` 记录扫描/重建任务、尝试次数和错误；领取任务时跳过已有运行扫描的来源。删除来源或对账删除文件时，同一事务清理引用它们的任务。

## 5. 查询管线

```text
parse(input) -> Expr(Term/Phrase/And/Or/Not)
strict pass -> HashMap<file_id, Evidence>
  Term/Phrase:
    recall  = FTS5 trigram OR 查询
              ∪ 双字 gram 和块边缘的首字候选
    verify  = 按查询长度扩展相邻 chunk，做严格字面查找
    own     = 命中归属到“命中起点所在 chunk”
    dedupe  = (file_id, owner_chunk_id, local_start)
  And = 交集并合并命中；Or = 并集；Not = 已索引文件全集减去严格子表达式
fuzzy fallback (mode == fuzzy, positive bare Term only)
  token -> 1/2/3-char gram tiers -> 候选词典词 -> 有界 Damerau-Levenshtein
        -> lexeme_occurrences -> 实际 chunk 字节区间
rank -> strict 分组 -> 请求排序键 -> offset/limit 分页
```

- 严格阶段在所有模式下都先执行。正文严格命中在 `exact`/`fuzzy` 模式计 12 分，`substring` 模式计 10 分；文件名精确/包含计 500/100。`FileResult.strict` 仅在整条布尔表达式严格成立时为 true。
- 只有正向裸 `Term` 进入 fuzzy 回退；`Phrase` 始终严格，`Not` 下的所有叶子以 `strict_only` 求值，避免用误拼反向排除文件。
- fuzzy 仅扩展至少 3 个 Unicode 字符的 token。最大编辑距离 1（3–5 字符）或 2（≥6 字符，硬上限 2），使用有界 optimal-string Damerau-Levenshtein；候选长度限制为目标长度 ± 距离。fuzzy 回退只补充距离 ≥1 的候选；距离 0 的精确词依赖严格阶段，严格阶段因 `case_sensitive` 未命中时 fuzzy 也不会补回。候选按共享 gram 分层，某层没有通过校验的词才回退到更短 gram 层；每层最多 4096 个候选词，每个词最多 1024 条 occurrence。
- fuzzy 命中只保留该 token 的最优距离词（最多 16 个），排除已有严格证据的文件，并对同一 chunk 内被更长 span 包含的短 span 去重。fuzzy 回退命中计 4 分；`matched_text`/`start`/`end` 指向原文实际区间，而不是查询拼写。
- 结果附加 `SearchHit.fuzzy`、`SearchHit.edit_distance`、`SearchHit.matched_lexeme`；fuzzy 命中保持实际 `body`/`ocr_body` 类型。界面显示“容错 · 距离 N”，查询计划提示“严格命中优先”。
- 排序先按 `strict` 分组（严格文件始终在前），组内再按相关度/时间/大小/路径/文件名/命中数排序，最后用路径和文件 ID 打破并列；分页在分组之后执行，翻页不会把 fuzzy 结果混入严格组。

### 跨块规则

- 相邻 chunk 只在 locator 连续时拼接：`Text.end_offset == next.start_offset`，PDF 同一页，DOCX 同一段落或同一表格单元。
- 不允许跨 PDF 页、跨 DOCX 段落/单元格拼短语。
- 单字倒排和块边缘候选覆盖没有完整双字或三字 gram 的跨块匹配。相邻块扩展受查询长度和定位边界约束。
- 排序在文件级筛选后执行，并用路径和文件 ID 打破并列。命中数按去重后的完整匹配计数，每个文件返回有限的用于展示的摘要。

## 6. 桌面搜索、设置与数据库维护

React 在 Enter 或点击搜索时提交参数快照，翻页沿用同一快照。Tauri 把数据库操作放到阻塞线程池，每个搜索使用独立连接与取消信号，界面按请求编号接收结果。

文本预览从 SQLite 读取命中块或文件的第一块缓存。PDF 原页由 PDFium 渲染为 PNG，页面版本须与索引一致，缓存文本可用于源暂时离线的情况。历史记录按 SQLite 设置写入，翻页沿用已有历史。

设置界面分为调度、来源、桌面字体和数据库维护：

- 调度：暂停/恢复、取消任务、进度、最近错误，以及节能/均衡/高速资源模式。
- 来源：扫描方式、递归、扩展名、排除目录/通配符、大小上限、轮询间隔和启用状态。
- OCR：总开关、语言（`chi_sim+eng`/`chi_sim`/`eng`）、并发页数 1–4、渲染 DPI 100–400、单页超时 1–180 秒。设置写入 SQLite `app_meta`，下一次扫描生效；默认值见 §7。
- 桌面设置保存到 `desktop.json`。默认关闭窗口即退出，开启托盘设置且托盘创建成功时会隐藏窗口。开机启动默认关闭，保存该设置的变更时更新当前用户的 `Run` 注册表项。
- 字体：Windows 用 `EnumFontFamiliesExW` 枚举系统字体；非 Windows 返回空列表，前端回退默认字体栈。界面字体、预览字体和 8–72 px 字号即时预览并持久化；缺失字体自动回退。
- 退出时通知调度器停止，并在可访问当前数据库时尝试 WAL checkpoint。未完成的任务留在数据库中。

数据库迁移与缓存清理是两条独立路径：

- 迁移目标必须是绝对路径、当前不存在、且没有 SQLite sidecar。`DatabaseLocation::migrate` 用 `VACUUM INTO` 复制到目标目录内的临时文件，依次执行 `integrity_check`、`foreign_key_check`、`verify_fts` 和 fsync，再以 `persist_noclobber` 发布，最后写入设置。旧数据库文件从不重命名、替换或删除；发布前冲突、目录 fsync 失败、sidecar 出现或设置写入失败都会删除已发布目标并继续使用旧库。迁移需要独占写租约，存在活跃搜索/索引时返回“数据库正忙”，界面建议先暂停索引。
- 清理正文缓存会先暂停调度器，随后在同一事务删除 `chunks`、清空 FTS、级联删除 `short_grams`/词典 occurrence、删除 `ocr_page_cache`、把 `indexed/stale/failed` 文件重置为 `pending` 并清空 `indexed_at`/`error_message`/`ocr_profile`，再删除索引任务；来源、文件元数据、搜索历史和磁盘原文保留。成功后调度器保持暂停，需手动恢复；失败则恢复操作前的暂停状态。重建索引为启用来源排队强制扫描。

## 7. 编码与格式

- TXT/MD/源码：BOM（UTF-8/UTF-16LE/BE）→ 严格 UTF-8 → 无 BOM UTF-16 启发式 → GB18030（GBK 超集）。解码错误会体现在编码标签 `gb18030-lossy`。
- 分块：目标 8 KiB，硬上限 16 KiB，优先在换行处切分；无重叠，避免重复命中。行号在切分时累计。
- DOCX：只读取 `word/document.xml`；同一段落内所有 `w:t` run 合并；表格单元单独成 section；不解析图片、隐藏文本、页眉页脚。
- PDF：PDFium 逐页取文本层；`PdfPage.page` 从 1 开始。每页原生文本分为 `Good`/`Empty`/`Sparse`/`Garbled`：`Good` 只保留原生文本，永不 OCR；其余页在启用 OCR 时按配置 DPI 渲染为 PNG 后送 Tesseract。OCR 成功时该页只索引 OCR 文本，不与原生文本合并；失败时保留非空原生文本作为 `Native` 回退，空页不产生 section。每个 section 仍保存页码 locator，混合 PDF 不会重复索引文字。
- 页面质量阈值：`Empty` 为无可见非空白字符；`Garbled` 为 U+FFFD 或非空白控制字符达到或超过 2%，或可见字符 ≥32 且字母/数字/CJK 占比低于 40%；`Sparse` 为有意义字符少于 `max(8, 页面面积 pt² / 50_000)`；其余为 `Good`。启发式无法识别“可打印但语义错误”的乱码。
- OCR 默认：启用，Tesseract，`chi_sim+eng`，250 DPI，60 秒/页，并发 2，单页 40,000,000 像素。配置中的 0 DPI/0 像素回退默认值，0 并发回退 2；并发硬上限 16。桌面设置将并发限制在 1–4、DPI 100–400、超时 1–180 秒。
- 渲染上限：目标宽度为页面宽度（pt）× DPI / 72，夹取到 50–10,000 px；同时受高度 16,384 px/aspect 和单页像素上限约束。页面过高或缩放到小于 1 px 时该页记为可重试失败，不分配超大位图。
- OCR 子进程参数直传（不过 shell），stdin/stdout/stderr 关闭，输出写入临时文件；超时后 `kill()` 并等待回收，避免管道阻塞。模型目录通过 `--tessdata-dir` 传入，不依赖进程级全局设置。PaddleOCR 是保留适配器，返回明确的“不可用”错误，不启动进程。
- `OcrExtraction.pages` 记录每页的质量分类、来源、OCR 状态、缓存键和字符数。`failed_pages` 记录 OCR 或渲染错误。缓存保留 OCR 成功的页，包括识别成功且没有文字的页，以避免每次扫描都识别空白页。
- 缓存键：`OCR_CACHE_SCHEMA` + 文件内容指纹（FNV-1a 128，非安全边界）+ 页码 + 引擎 id/适配器版本 + 语言 + DPI + 像素上限 + 引擎/模型文件修订。`ocr_profile` 使用同一配置身份（不含页码/内容），只有全部页成功才写入；未变化文件可在 profile 匹配时跳过重新提取。
- 运行库查找不依赖 CWD：Tesseract 显式含目录路径 > `TESSERACT_PATH` > EXE 目录的 `ocr/tesseract.exe`、`ocr/tesseract`、`tesseract[.exe]`、`bin/tesseract[.exe]`、`tesseract/tesseract[.exe]` > PATH 上的 `tesseract`。模型目录显式 `tessdata_path` > `TESSDATA_PREFIX` > EXE 目录的 `ocr/tessdata`、`tessdata`、`share/tessdata`。
- CLI `index` 默认按已保存设置启用 OCR；`--no-ocr` 本次关闭，`--ocr` 覆盖已保存的关闭状态，`--tesseract`/`--lang` 覆盖本次可执行文件和语言。PDF 仍按逐页质量选择性 OCR。

## 8. Windows 便携构建

- `scripts/build-windows.sh` 使用 `x86_64-pc-windows-gnu` + MinGW 交叉编译 CLI 与 GUI，输出 `target/IndexFox-windows-x64-portable.zip`。构建前需执行 `(cd app && npm run build)`。
- `scripts/build-tesseract-windows.sh` 从固定源码构建 Windows x64 Tesseract 5.5.4：Leptonica 1.87.0、libpng 1.6.59、zlib 1.4 静态链接，`-static -static-libgcc -static-libstdc++`；只启用 PNG 输入，关闭 TIFF/JPEG/GIF/WebP/OpenJPEG、training、libarchive/curl 和 OpenMP。产物在 `.deps/windows/tesseract-minimal/`，PE 导入表仅含 Windows 系统 DLL，不携带 UB Mannheim 的 DLL 闭包。
- 便携 ZIP 根目录包含两个 EXE、`WebView2Loader.dll`、`pdfium.dll`、发行说明和逐文件哈希清单。OCR 和 WebView2 分别位于 `ocr` 和 `runtime/webview2`。第三方许可证位于 `licenses`。
- `scripts/build-windows.sh` 默认使用 `.deps/windows/tesseract-minimal`（可用 `TESSERACT_RUNTIME_DIR` 覆盖），校验该目录的 `SHA256SUMS.txt`/`BUILD-INFO.txt`/许可证，再复制 `tesseract.exe`、模型与配置；不复制任何 UB Mannheim DLL 或许可证缓存。
- Fixed WebView2 必须完整解包到 `runtime/webview2`，即 `runtime/webview2/msedgewebview2.exe` 直接可读；`app/src-tauri/src/main.rs` 只探测该根层并设置 `WEBVIEW2_BROWSER_EXECUTABLE_FOLDER`，不读取注册表。`SKIP_WEBVIEW2=1` 只用于依赖系统 Evergreen 的开发 ZIP，不是发布形态。
- GUI 必须同时打包 `WebView2Loader.dll`；缺少时进程会启动但不创建窗口。`tauri` 需要 `--features custom-protocol` 才会嵌入 `app/dist`，否则会尝试加载 `http://localhost:1420`。
- 包内静态 OCR 和 Fixed WebView2 已在 Windows 11 非提权模式下验证。

## 9. 验证状态与边界

- Rust workspace 122 项测试和独立调度与存储模块 10 项测试通过，覆盖扫描中止、`SQLITE_FULL` 回滚、词典回填、完整标识符容错、空白 OCR 页缓存和失败页重试。
- 需要真实 PDFium/Tesseract 的 M4 用例在缺少 `PDFIUM_LIB_PATH`/`TESSERACT_PATH` 时会显式跳过，不应把跳过计为真实 OCR 通过。
- M0–M3 的 Windows 11 实机验证保持原有范围：连续写入、SMB/WSL UNC 离线恢复、暂停/取消、迁移回滚、字体和托盘、退出后恢复重建任务。外部 NAS 真断网、写满物理磁盘等未测项仍见[开发任务](02_Codex_开发任务.md)。
- 性能、准确性和 OCR 单页耗时见[基准报告](BENCHMARK.md)。
- Windows 解压冒烟使用私有 `%TEMP%` 数据目录，验证包内运行时、OCR 和容错。另对 WebView2 页面模拟离线并验证桌面查询、原页和高亮。干净 Windows 镜像、Windows 10 和系统断网未实测。 [开发任务](02_Codex_开发任务.md)
- 边界：PDF 仍只有页码 locator，无 bbox/字形坐标；PaddleOCR 明确不可用；fuzzy 只处理 ≥3 字符的正向裸 term，短语、`NOT`、文件名保持严格，距离硬上限 2，长 token/CJK 受 64 字符和候选/occurrence 上限约束；OCR 质量分类是启发式，缓存指纹不是安全边界。
