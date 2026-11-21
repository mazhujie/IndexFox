# IndexFox 查询语法

用户输入先解析为 AST，绝不会直接作为 FTS5 查询语言执行。严格路径中 FTS5 只负责候选召回，fuzzy 回退改用独立词典召回；所有命中都要在原文或已索引的实际区间上复核。

## 1. 语法

```text
expr    := or
or      := and (OR and)*
and     := unary (AND? unary)*      # 相邻表达式是隐式 AND
unary   := NOT unary | '-' unary | primary
primary := '(' expr ')' | '"' phrase '"' | term
```

| 输入 | 含义 |
|---|---|
| `Linux 驱动` | `Linux AND 驱动` |
| `Linux OR Windows` | 文件级 OR |
| `Linux -Windows` | `Linux AND NOT Windows` |
| `"Linux kernel"` | 连续短语 |
| `(Linux OR Windows) 驱动` | 括号分组 |
| `dma_alloc_coherent()` | 整个函数名是字面 term |
| `#include` | `#` 是内容字符 |
| `100%` / `a_b` | `%`、`_` 不是 SQL 通配符 |

- `AND`、`OR`、`NOT` 仅大写时是运算符；小写 `and` 等按普通词处理。
- 前导 `-` 表示排除；词中间的 `-`（如 `Linux-Windows`）保留为字面字符。
- 括号紧跟在词后时属于字面内容（函数调用）；表达式分组括号前面必须是空白或输入开头。
- 双引号短语支持 `\"` 和 `\\` 转义；未闭合引号会报错并给出起始位置。

## 2. 错误位置

解析错误包含 0 基字节位置：

- 未闭合 `(`：指向 `(`。
- 未闭合引号：指向开引号。
- `OR`/`NOT`/`-` 后缺少表达式：指向运算符。
- 多余 `)`：指向该字符。

CLI 输出错误文本。桌面界面按字节位置高亮出错字符，并接受最长 4096 UTF-8 字节的表达式。

## 3. 模式与大小写

| 模式 | 行为 |
|---|---|
| `exact` | 严格连续子串；可选择大小写敏感。 |
| `substring` | 任意位置子串；默认仅 ASCII 大小写不敏感。 |
| `fuzzy` | 先执行同一套严格匹配，再对正向裸 term 做词典编辑距离回退；不会静默降级。 |

`exact` 与 `substring` 共享同一套严格核对逻辑，区别只体现在用户语义和评分；二者都不会返回编辑距离近似结果。

fuzzy 规则：

- 只有非引号、非否定的裸 `Term` 进入回退。`"phrase"` 始终严格；`NOT`/前导 `-` 下的所有表达式按严格求值，因此否定的误拼不会反向排除文件。文件名匹配在所有模式下保持严格。
- 严格阶段先完整执行。`FileResult.strict` 仅当整条布尔表达式在该文件严格成立时为 `true`；严格正文命中在 `exact`/`fuzzy` 计 12 分、`substring` 计 10 分，文件名精确/包含计 500/100。fuzzy 回退命中计 4 分。
- 所有排序键都先按 `strict` 分组：严格文件始终在前，组内再按相关度/时间/大小/路径/文件名/命中数排序；分页在分组后执行。
- fuzzy 回退是 ASCII 大小写不敏感的（词典键已规范化）；`case_sensitive` 只作用于严格阶段。
- 结果解释字段：`FileResult.strict`，`SearchHit.fuzzy`、`SearchHit.edit_distance`、`SearchHit.matched_lexeme`。strict 命中的 `edit_distance` 为空；fuzzy 命中给出实际距离和命中的词典词。`matched_text`/`start`/`end` 指向被命中的原文实际区间，而不是查询拼写；fuzzy 命中仍保留实际 `body`/`ocr_body` 类型，界面显示“容错 · 距离 N”。

文件级 AND/OR/NOT：不同位置分别命中两个词也算 AND 命中；每个结果列出具体命中位置。

## 4. fuzzy 词典与上限

- 词典词：完整 `[A-Za-z0-9_]+` token，加 `_`、camel-case、字母/数字边界拆出的子串（拆分子串最短 2 字符）；CJK 连续串保留 ≤64 字符的完整串，并生成 2..=4 字符重叠窗口。
- 查询中的字母数字标识符作为整词容错，独立的纯数字按字面匹配。可以单独搜索标识符中已入词典的片段。
- fuzzy 只扩展 ≥3 个 Unicode 字符的 token。最大编辑距离：3–5 字符为 1，≥6 字符为 2（硬上限 2），使用有界 optimal-string Damerau-Levenshtein；候选词长度限制在目标长度 ± 最大距离。距离 0 的精确词由严格阶段负责，不重复计入 fuzzy 回退。
- 候选 tier 依 token 长度选共享 gram：3 字符用 `2,1`，4–5 字符用 `3,2,1`，≥6 字符用 `3,2`。某 tier 没有通过距离校验的词才继续下一 tier；不引入全表 chunk 扫描。
- 上限：每个 chunk 最多 4096 个词典词；每词最多 96 个去重 1/2/3 字符 gram；每个 tier 最多 4096 个候选词；每个词最多 1024 条 occurrence；每个裸 term 最多展开 16 个查询 token，每个 token 只保留最优距离的最多 16 个词，待处理命中上限 4096。
- fuzzy 回退只补充距离 ≥1 的候选；距离 0 的精确词依赖严格阶段，严格阶段因 `case_sensitive` 未命中时 fuzzy 也不会补回。
- 不支持：2 字符及以下的 fuzzy、距离 >2 的近似、fuzzy 文件名、把引号短语或否定项做近似；词典词的完整串最长 64 字符，更长的查询 token 不保证召回。候选/occurrence 上限可能丢弃极常见 gram 的尾部匹配。

## 5. 召回与核对

- 至少 3 个非空白字符时，用 FTS5 trigram、双字 gram 和块边缘首字倒排召回。
- 1–2 个非空白字符用首字倒排召回。纯空白引号短语从包含其首字符的块召回。
- 候选 chunk 按查询长度拼接连续相邻块并核对原文。命中归属到起点所在 chunk。
- 跨 PDF 页、跨 DOCX 段落/单元格不拼接，避免伪造连续短语。
- 文件名子串用 `instr()`（或 `instr(lower(...))`）匹配，`%`/`_` 保持字面。
