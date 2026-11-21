# IndexFox V1.0 — Codex 分阶段开发任务

> 适用范围：Windows x64、Tauri 2 + Rust + SQLite FTS5 + PDFium + Tesseract。**不要直接在一个任务里实现整个项目。**

> **产品名：IndexFox（索引狐）**。主程序 `IndexFox.exe`；仓库目录 `indexfox/`；Rust crate 统一使用 `indexfox-` 前缀；默认数据目录 `%LOCALAPPDATA%\IndexFox\`。Tauri 应用标识暂定 `com.indexfox.desktop`，发布前核验唯一性。详见 `01_技术设计方案.md` 第 0 节。

## 0. 交付规则

1. 先阅读 `docs/01_技术设计方案.md`，21项既定需求及 IndexFox 命名约定不可擅自更改。
2. 每个任务提供：设计/实现说明、可运行测试、示例命令、局限性、修改文件清单。
3. 只在已验收的阶段上继续；不可因为当前某项较难就静默取消短词检索、跨块搜索或离线保留。
4. 尽量使 Rust 核心独立于 UI，便于 CI/本机运行纯 CLI 测试。
5. SQLite schema 采用迁移（migration）；不在旧索引上直接无备份破坏性升级。
6. 任何用户文档只读操作；不上传网络，不强制管理员权限。
7. 有依赖许可证/分发问题需形成 `THIRD_PARTY_NOTICES.md`，不可忽略。
8. 未经基准测试，不得声称特定查询延迟、索引大小或 OCR 准确率。

## 1. 仓库结构规范

```text
indexfox/
├── README.md
├── docs/
│   ├── 01_技术设计方案.md        # V1.0 技术设计方案（21项确认需求与总体架构）
│   ├── 02_Codex_开发任务.md      # M0–M4 分阶段开发任务与交付规范
│   ├── 03_验收测试矩阵.md        # Q01–Q32 验收测试矩阵（最小集）
│   ├── ARCHITECTURE.md          # 模块实现架构细化文档（开发阶段维护）
│   ├── QUERY_SYNTAX.md          # 查询语法与 AST 规范说明
│   ├── BENCHMARK.md             # 基准测试记录报告
│   └── prototype/
│       └── index.html           # V1.0 交互式 UI 原型（离线单文件）
├── crates/
│   ├── indexfox-core/           # 核心模型、配置、统一接口
│   ├── indexfox-source/         # 扫描器、监控器、对账
│   ├── indexfox-extract/        # txt/md/docx/pdf/ocr adapters
│   ├── indexfox-index/          # SQLite schema、FTS5、短词、词典
│   ├── indexfox-query/          # AST、planner、匹配校验、排序
│   └── indexfox-cli/            # 验证与基准 CLI
├── app/
│   ├── src/                     # React/TypeScript 前端视图与字体/主题状态
│   └── src-tauri/               # Tauri 命令、系统字体枚举和托盘
├── fixtures/                    # 生成的测试样本，勿放个人资料
├── tests/                       # 端到端与回归测试
└── scripts/                     # 构建、便携打包与故障注入脚本
```

## 2. 任务清单

### M0 — 环境和技术风险验证
- [ ] `M0.1` Rust workspace + Tauri 最小空窗口（标题 `IndexFox · 索引狐`）+ `IndexFox.exe` 的 Windows x64 ZIP 试运行。
- [ ] `M0.2` 检查 WebView2 运行时缺失时的行为；研究附带 Fixed Runtime 的运行时路径配置并写清兼容性。
- [ ] `M0.3` SQLite FTS5 trigram 演示：`VIDIOC_STREAMON`、中文3字、大小写、`%`、`_`、标点、短于3字符查询限制。
- [ ] `M0.4` PDFium 读取多页 PDF、提取文字并渲染指定页。
- [ ] `M0.5` Tesseract 对中文+英文扫描页 OCR，能够单独结束超时 OCR 子任务。
- [ ] `M0.6` 使用 Windows 本地、SMB、WSL UNC 进行可访问性和路径差异验证；无法在测试环境验证的场景明确标记。

**M0 验收**：无 GUI 也能完成 PDF/文本抽取与 CLI 子串检索；Windows ZIP 可启动且识别测试文件；给出依赖分发清单。

### M1 — 核心数据库与提取
- [x] `M1.1` `sources/files/chunks/chunks_fts/short_grams/search_history/index_jobs` 迁移、事务与 CRUD。
- [x] `M1.2` TXT/MD/源码编码识别、行号/偏移映射与文本分块。
- [x] `M1.3` DOCX 段落、表格、跨 run 文本拼接；损坏 ZIP/缺失 XML 安全失败。
- [x] `M1.4` PDF 按页提取文本层，记录页码；本阶段先不包含完整 OCR。
- [x] `M1.5` 单文件更新原子替换 chunks、FTS5 和 short_grams；失败旧索引保留并标 stale。
- [x] `M1.6` 单字/双字辅助 gram 索引构建和查询；设诊断指标观察数据库体积（`stats` 输出 `short_grams` 计数与 `page_count × page_size` 逻辑库大小）。
- [x] `M1.7` CLI 索引一个目录；100MB 规则、格式过滤、忽略规则与元数据检索。当前最低 CLI 验收：`index <ROOT> [--file <相对路径>]`（单文件补索引不对账、不改动其他记录）。

**M1 验收用例**：`内`、`内核`、`内核驱动`、`V4`、`VIDIOC_STREAMON`、`dma_alloc_coherent()`、`#include`、GBK 中文、UTF16、DOCX 表格、跨 run、跨 chunk、PDF 页码；与原文真值逐条核对。

M1 已通过核心验收。Linux 配置 PDFium 和 Tesseract 路径后运行 `cargo test --workspace --offline`，48 项通过，包含真实 PDF 页码与中英文 OCR。旧版 SQLite v1 样本库可直接查询，末尾换行的结束行在生成搜索结果时按缓存文本校正。

### M2 — 查询语法与 Windows GUI
- [x] `M2.1` AST parser：隐式 AND、OR、NOT、括号、双引号短语；非法表达式显示出错位置。
- [x] `M2.2` 文件级布尔组合，SQL 参数绑定，大小写切换、全文与文件名候选合并。
- [x] `M2.3` 精确及子串查询校验器，处理 `%/_/-/#/()`、空白与跨分块匹配。
- [x] `M2.4` 相关度评分与可选修改时间、大小、路径、文件名、命中数排序。
- [x] `M2.5` Tauri 搜索框 Enter/搜索按钮；无实时搜索；任务取消、分页、耗时显示。
- [x] `M2.6` 左侧目录树、中间结果与命中摘要、右侧 TXT/MD/DOCX 文本预览；关键字高亮、定位行/段落。
- [x] `M2.7` PDF 原页预览和页码定位；无 bbox 时仅高亮检索摘要，不伪造页面精确位置。
- [x] `M2.8` 历史100条、关闭/清空和结果上下键选择。

**M2 验收**：10万条虚拟文件元数据时 UI 不被阻塞；结果分页切换正常；表达式与高亮测试通过；重复 Enter 不混合新旧结果。

Windows 11 + WebView2 Evergreen 实测通过。隔离数据库包含 100,000 条虚拟元数据和 8 个合成文件。`NOT missing` 查询期间可打开历史，前两页各 50 个文件且无重叠。取消后可继续查询，连续提交只显示最后一次结果。中文高亮、PDF 第 2/3 页原图和历史关闭、开启、清空均已验证。Rust workspace 64 项测试通过，前端运行 `npm run build`，Windows 便携包由 `scripts/build-windows.sh` 构建。

### M3 — 索引调度、可靠性与设置
- [x] `M3.1` 每源任务配置、扫描 generation、增量对账、防离线误删。
- [x] `M3.2` notify 事件合并和本地轮询回查，SMB/WSL 定时扫描与断线重试。
- [x] `M3.3` 文件解析前后 mtime/size 一致性检查、改名处理、重叠目录去重、symlink/junction 环路保护。
- [x] `M3.4` 节能/均衡/高速三档并发控制，暂停/恢复/取消与进度显示。
- [x] `M3.5` 用户自定义数据库路径、迁移/校验/回滚，缓存清理与索引重建。
- [x] `M3.6` 关闭即退出/托盘驻留配置、退出时 checkpoint、开机启动默认关闭、系统字体枚举、界面和预览字体及字号持久化（缺失字体自动回退）。
- [x] `M3.7` 源状态 `online/offline/scanning/failed` 在界面中区分，断连时保留缓存索引。

**M3 验收**：拔断 SMB、关闭 WSL、文件快速连续写入、任务中断、磁盘不足、数据库恢复、迁移中途失败等场景均可恢复且不丢原文文件；设置中读取系统字体并修改界面/预览字体即时生效且重启保留。

Windows 11 与 WebView2 Evergreen 实机通过快速连续写入、暂停时取消任务、默认退出、托盘驻留继续索引、数据库迁移与目标冲突回滚、缓存清理与重建，以及字体和字号即时生效与重启保留。关闭应用后重启，可恢复 1,200 个文件的重建任务。本机 SMB 与 WSL UNC 通过移走和恢复隔离目录验证离线保留与恢复对账。Rust workspace 91 项测试和调度与存储模块的 10 项独立测试通过，覆盖中断任务恢复、设置写入失败时的迁移回滚和 `SQLITE_FULL` 故障注入。

外部 NAS 实际断网、停止 WSL 实例、写满物理磁盘和登记开机启动未实测。Windows Tauri 单元测试程序未能启动，实际打包的应用已在 Windows 11 上验证。M3 首次打开 v1 数据库时自动升级到 v2，M2 无法打开已升级的数据库。

### M4 — OCR、拼写容错与打包
- [x] `M4.1` PDF 逐页判断文字质量并自动 OCR，支持配置 DPI、并发和超时，限制页面像素，随包提供中英文模型。
- [x] `M4.2` 逐页缓存、页码映射和失败页重试，默认 Tesseract，预留 PaddleOCR 接口。
- [x] `M4.3` 独立词典、英文和代码标识符提取、中文候选特征、编辑距离校验、阈值和原文高亮。
- [x] `M4.4` 容错与 AND/OR/NOT/短语组合回归通过，严格匹配在各排序键下优先。
- [x] `M4.5` 混合型 PDF 仅对需要的页做 OCR，各页只保留一份文本，命中可定位原页。
- [x] `M4.6` 完成 1 万和 10 万文件合成基准，记录准确性、P50/P95、索引体积和峰值内存。 [基准报告](BENCHMARK.md)
- [x] `M4.7` Windows x64 便携 ZIP 包含 EXE、PDFium、静态 Tesseract 5.5.4、chi_sim/eng、Fixed WebView2、许可证、SHA256 清单和 README。
- [ ] `M4.8` 已在 Windows 11 中完整测试非提权令牌和新建隔离用户数据目录，仍需全新 Windows 系统与系统断网验证。

**M4 验收**：无网络下完成扫描 PDF 中文/英文、拼写错误查询、搜索结果预览；最终 ZIP 在干净 Windows x64 环境中运行，全部21项需求按测试矩阵验证。

Rust workspace 122 项和独立调度、存储模块 10 项测试通过。Windows 11 的隔离解压冒烟覆盖自动加载包内运行时、中英文 OCR、Q10 容错与混合 PDF 页码。将 WebView2 页面置为离线后，桌面端仍可索引、查询、高亮容错原文和 OCR 文本，并打开 PDF 第 4 页原图。关闭 OCR 后复扫只索引 PDF 原生页，重启仍保留设置。

发行包可从本地目录直接运行。首次打开旧数据库会自动升级到 v4，旧版程序无法打开升级后的数据库。操作系统未整体断网，测试环境未提供全新 Windows 镜像。后续操作和通过标准见[待实机验证清单](03_验收测试矩阵.md#待实机验证)。

## 3. 建议给开发 Agent 的第一个开发指令

```text
本项目正式名称为 IndexFox（索引狐），请统一使用 indexfox 仓库名、indexfox-* crate 命名、IndexFox.exe 主程序名和 %LOCALAPPDATA%\IndexFox 默认数据目录。
请先阅读 docs/01_技术设计方案.md、docs/02_Codex_开发任务.md、docs/03_验收测试矩阵.md 以及 UI 交互原型 docs/prototype/index.html，仅执行 M0：建立 Rust workspace、最小 Windows Tauri 2 GUI、SQLite FTS5 trigram + 单/双字检索技术验证、PDFium/Tesseract 可用性与 Windows x64 便携发布验证。

不要提前实现全套 GUI、后台任务管理或拼写容错。不要修改 21 项既定需求约束。

输出：目录结构、Cargo/Tauri 配置、可运行的原型、单元测试/示例、运行方法、第三方依赖清单、尚未解决的分发问题。对于未测试的平台和依赖，不得标记为已验证。
```

## 4. 测试数据生成原则

- 用合成数据或开源样本，不把个人文件上传到公开仓库。
- 创建中文、英文、混合字符、GBK、UTF-16、长文本、多页 PDF、扫描 PDF、混合 PDF、DOCX 表格和跨 XML run 的样本。
- 保存 oracle：输入查询 → 预期命中 file_id/页码/行号/高亮区间；算法改动必须与 oracle 对比。
- 对网络源断连/迁移等使用脚本注入故障，并记录重现方法。

## 5. 阻塞问题需在技术验证阶段报告，不另行打断用户确认

- 可合法分发的 PDFium/Tesseract/模型/WebView2 二进制来源、版本与 SHA256。
- Tauri 便携运行模式下 Fixed WebView2 Runtime 的实际路径配置。
- SQLite 1/2 字符索引体积与中文拼写词典的召回/内存权衡。
- 特殊 DOCX 对象/隐藏文本、扫描 PDF 有效性判定标准。
