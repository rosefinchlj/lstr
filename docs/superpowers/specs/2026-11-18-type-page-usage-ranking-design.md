# 类型页使用学习排序设计（先全量、后打分、再截断）

日期：2026-11-18
状态：已获用户批准（2026-11-18），待实施

## 背景与目标

lstr 已有完整的学习机制：`History` 记录每一次打开（frecency）以及「某个查询词下选了哪条」
（query_hits），`rank::rank_with` 在最终排序里同时用这两份信号。但**类型页自己的候选窗口**
是在打分之前按名称 / 修改时间截出来的，导致「用得多的冷门条目」永远进不了窗口，也就永远
升不到前面。用户反馈：应用、书签、笔记等类型页都应该像「最近使用」一样，**最近用过、用得
多的排前面**。

用户补充（2026-11-18）：原话里的「标签」是「书签」的笔误；需要学习机制的类型页是
**应用、书签、笔记**（命令页顺带保持一致）。

目标：

1. 所有类型页（应用 / 书签 / 笔记 / 命令）的「空输入列表」按使用历史排序：用过的在前
   （frecency 降序），没用过的保持原有自然顺序；
2. 类型页的「搜索命中列表」不再在打分前截断，frecency 与 query_hits 都能把候选从后面提上来；
3. 「全部」页里的非文件来源（应用 / 命令 / 书签 / 笔记）同样全量参与最终打分；
4. 不新增学习表、不改数据库结构、不做迁移。

非目标（本轮不做）：

- 按「标签页使用频率」调整顶部 tab 顺序；
- 新增 tag 实体 / 标签功能（用户已澄清原文「标签」是「书签」的笔误）；
- 改变 frecency 公式本身；
- 跨设备同步学习数据。

## 现状（代码事实）

- 空输入分支 `src/search.rs:2646`：
  - `Scope::App` → `app_scope_entries`（`src/search.rs:3192`）已按 usage 分档：用过的在前、
    按 frecency 降序，没用过的按内置常见名单 + 名称兜底。**应用页空输入已经满足目标**。
  - `Scope::Command` → `commands("")` 返回全部命令，条目本来就全量进 finalize 打分。
  - `Scope::Bookmark` → `bookmarks.top(request.deliver.max(request.limit))`；`BookmarkIndex::top`
    （`src/search/bookmarks.rs:93-95`）是「名称序取前 N」，注释明确「不掺历史/常用排序」。
  - `Scope::Note` 不走这条分支，提前交给 `notes_response`（`src/search.rs:2624`）；空输入 →
    `notes.top(take)`（`src/search/notes.rs:119-125`），是「修改时间序取前 N」，注释为
    「空输入『笔记』页按最近修改排序」。
- 有输入分支 `src/search.rs:2734`：
  - `Scope::App`：`apps.search(text, usize::MAX)` + 选项过滤后 `matches.truncate(take)`
    （`src/search.rs:2751`）；
  - `Scope::Bookmark`：`bookmarks.search(text, usize::MAX)` + 复核后 `hits.truncate(take)`
    （`src/search.rs:2772`）；空文本 + 非空筛选也走 `bookmarks.top(take)`（`src/search.rs:2757-2759`）；
  - `Scope::All`：应用 / 书签 / 笔记在并入 `entries` 前各自 `truncate(source_take)`
    （`src/search.rs:2889`、`2911`，笔记在 `2941` 附近）；文件候选仍按 `fetch_count` 预算截断。
- 最终排序：`finalize_entries_with_pending` 在 `src/search.rs:1518-1525` 调
  `rank::rank_with(...)`，随后才在 `src/search.rs:1660` 执行 `entries.truncate(deliver)`。
  空查询打分是 `history_points + launch_points + kind_preference + folder_points`
  （`src/search/rank.rs:180-182`），其中 `history_points = (score*10).round().clamp(0, history_cap)`
  （默认上限 150），排序是稳定排序。
- 笔记页非空分支 `notes_response`（`src/search.rs:1900-1921`）：`limit = take`（严格模式才
  `usize::MAX`），`notes.search` 内部先 `rank_with` 再截断到 `limit`，返回后 `hits.truncate(take)`。

**根因**：类型页的「候选窗口」由名称序 / 修改时间序 / `take` 在打分之前决定，学习信号只能
在窗口内部重排，窗口外的常用条目没有机会进入结果。

## 设计

### 1. 核心不变量

> 类型页的候选必须在**打分之前**保持全量；截断只允许发生在最终交付处
> （`finalize` 的 `truncate(deliver)` 或 `notes_response` 的 `truncate(take)`）。

「全量」的边界：应用（几百条）、命令（几条）、书签（读取上限 20k）、笔记（上限 20k）。
这些都是内存索引，全量打分代价可忽略；文件候选（几十万条）**不受影响**，继续用现有预算。

### 2. 空输入类型页

新增两个薄封装（各自模块内可单测）：

- `BookmarkIndex::used_page(&self, history: &History, take: usize) -> Vec<Entry>`
- `NoteIndex::used_page(&self, history: &History, take: usize) -> Vec<Entry>`

两者都调用 `rank` 模块的新公共函数：

```rust
pub fn usage_page<'a>(
    items: impl IntoIterator<Item = &'a Entry>,
    history: &History,
    take: usize,
) -> Vec<Entry>
```

语义：

1. 用过的条目（`rank_snapshot("").score(id) > 0.0`）排在前面，按 score 降序；
2. 没用过的条目保持调用方给的自然顺序（书签 = 名称升序，笔记 = 修改时间降序）；
3. 只取前 `take` 条，且只 clone 这 `take` 条（不整表 clone）；
4. score 相同的按自然顺序稳定排列。

实现要点：先收集 `Vec<&Entry>`，只对「used 子集」排序（用过的通常几十条，O(u log u)），
再按自然序补足未用条目，避免对 20k 笔记整体排序。

调用点：

- 书签页空输入（`src/search.rs:2726-2731`）与「空文本 + 非空筛选」分支
  （`src/search.rs:2757-2759`）都改用 `bookmarks.used_page(&history, take)`，
  `total` 仍为 `bookmarks.count()`；
- 笔记页空输入（`src/search.rs:1897-1899`）改用 `notes.used_page(&history, take)`，
  `total` 仍为 `notes.count()`；
- 应用页空输入**保持现状**（`app_scope_entries` 已经用 usage 分档，且处理了收藏置顶 /
  隐藏应用 / Ctrl+H 排序，不值得为统一而重构）；
- 命令页**保持现状**（`commands("")` 本来就全量进 finalize，空查询打分已把用过的排前）。

### 3. 有输入的类型页

- `Scope::App`（`src/search.rs:2743-2752`）：删掉 `matches.truncate(take)`，全量进 finalize；
  `total` 仍是全量匹配数；最终交付由 finalize 的 `truncate(deliver)` 负责。
- `Scope::Bookmark`（`src/search.rs:2763-2773`）：删掉 `hits.truncate(take)`；其余不变。
- `Scope::Note`（`notes_response`）：非空分支不再用 `take` 作为 `notes.search` 的 `limit`
  上限，改传 `usize::MAX`（严格模式原本就是）；`notes.search` 内部 rank 后返回全部命中，
  `notes_response` 末尾再 `truncate(take)`。这样 frecency / query_hits 能作用到全部命中。
- `Scope::All` 的非文件来源：`source_fetch` 改为 `usize::MAX`（不再受 `source_take` 限制），
  并删掉 `src/search.rs:2889`、`2911` 及笔记处的 `truncate(source_take)`。文件候选的
  `fetch` / `folder_fetch` / `file_fetch` 预算**保持不变**。

### 4. 排序信号与兼容性

- 学习信号不新增任何东西：`history_points`（frecency ×10，封顶 150）+ query_hits boost
  （封顶 240）都已在 `rank_with` 里，本轮只是让它们看到全部候选。
- 空输入页先 `usage_page` 再进 finalize，finalize 会以空 needle 再打一次分：同类型条目的
  `launch_points` / `kind_preference` / `folder_points` 相同，`history_points` 在 150 封顶后
  对高分条目并列——稳定排序会保持 `usage_page` 给出的次序（用过的恒在未用过的之上，因为
  未用过 score=0）。因此两段排序不会互相打架，无需给 `usage_page` 加特例。
- 收藏置顶（`pin_favorites`）、精确同名置顶、按 id 小写去重、同名保留规则全部不变。
- `total` 口径不变（仍为全量匹配数 / `count()`），`entries` 只交付 `deliver` 条，UI 的
  「加载更多」继续按 `total` 工作；「全部」页的 `total` 会变大（非文件来源不再只数窗口内
  的条数），这是修正而非回归。

### 5. 边界与回退

- `history` 互斥锁获取失败（被 panic 毒化）：`used_page` 退化为 `top(take)`，与现状一致；
- 书签 / 笔记功能关闭：沿用现有 `enabled` 判断，返回空；
- 删除过的书签 / 笔记：`usage_page` 只遍历索引内的条目，历史里的孤儿 id 不会出现在列表里；
- 历史最多 1000 条，`rank_snapshot("")` 一次读出，代价可忽略；
- 全量候选后 `App` / `Bookmark` 页的 `entries` 可能达到几百 / 几千条，`finalize` 会多做
  一轮打分；书签/笔记总量上限 20k、应用几百条，实测可接受；如线上出现卡顿，再把
  「全量」收紧为「`take` 的若干倍 + 用过的条目必进」两段式（本轮不做）。

## 测试计划

1. `src/search/rank.rs` 单测：`usage_page` 把用过的条目按 frecency 降序排前、未用条目保持
   自然序、`take` 生效、同分稳定。
2. `src/search/bookmarks.rs` 单测：用 `index_from` 构造 Alpha/beta/gamma，记录 beta 的使用
   历史，`used_page(history, 2)` 必须返回 `[beta, Alpha]`——即名称序窗口（Alpha, beta）之外
   的常用书签能被提到第一。
3. `src/search/notes.rs` 单测：构造修改时间递增的笔记，记录其中「最旧」那条的使用历史，
   `used_page(history, 2)` 必须把旧笔记排第一。
4. 现有 `cargo test` 全绿（含 `top_is_name_sorted_and_limited`、`bookmark_kind_scope_and_history_semantics`、
   `app_scope_*` 等回归）。
5. 手动验证：书签页 / 笔记页点开几条冷门项 → 重开对应 tab，确认它们出现在首屏；应用页输入
   关键词，确认常用项在同匹配档位里靠前；「全部」页搜索后确认冷门但常用的书签能出现。

## 实施清单

- `src/search/rank.rs`：新增 `usage_page` + 单测；
- `src/search/bookmarks.rs`：新增 `used_page` + 单测；
- `src/search/notes.rs`：新增 `used_page` + 单测；
- `src/search.rs`：
  - 空输入书签分支 / 空文本+筛选分支改 `used_page`；
  - 非空 App、Bookmark 删除 `truncate(take)`；
  - `notes_response` 空输入改 `used_page`，非空分支 `limit` 传 `usize::MAX`；
  - `Scope::All` 非文件来源 `source_fetch = usize::MAX` 并删除 `truncate(source_take)`；
- `README.md`：「书签」「笔记」页空输入说明补一句「按最近使用排序（学习）」；
- 完成后补「实施记录」小节，并跑 `cargo test` / `cargo build`。
