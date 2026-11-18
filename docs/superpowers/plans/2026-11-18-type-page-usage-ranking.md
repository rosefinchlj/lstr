# 类型页使用学习排序 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 应用 / 书签 / 笔记类型页先用全量候选做学习排序，再按 deliver 截断，让「最近用过、用得多」的条目稳定排在前面。

**Architecture:** 在 `rank` 模块新增通用的 `usage_page`（用过的按 frecency 降序在前，其余保持自然序，只取前 N 条），由 `BookmarkIndex::used_page` / `NoteIndex::used_page` 薄封装；同时删除 `search.rs` 里应用 / 书签 / 「全部」页非文件来源在打分前的 `truncate(take)`，让既有的 `rank_with`（frecency + query_hits）看到全部候选，最终只由 `finalize` / 响应层按 deliver 截断。

**Tech Stack:** Rust 1.85（edition 2021）、egui/eframe 0.32、SQLite（rusqlite）历史库、内置文件索引 `lstr-index`。项目是 Windows 单文件启动器，`src/main.rs` 为唯一 crate 入口。

## Global Constraints

- 只在 Windows 上构建/测试；不新增任何依赖。
- 学习信号复用现有 `History`（frecency + query_hits），不改数据库结构、不做迁移。
- 注释与 README 文案用中文，风格与相邻代码一致。
- 所有现有测试必须保持通过；每个任务结束前跑对应测试，最后一次提交前跑全量 `cargo test`。
- 文件候选（几十万条）的预算 `fetch_count` / `folder_fetch` / `file_fetch` 不得改动，只放开应用 / 书签 / 笔记 / 命令这类小索引。

---

### Task 1: `rank::usage_page` 通用排序

**Files:**
- Modify: `lstr/src/search/rank.rs`（在 `score_entry_planned` 之后、`drive_points` 之前插入；测试加在文件末尾的 `mod tests`）
- Test: `lstr/src/search/rank.rs` 内联 `mod tests`

**Interfaces:**
- Produces: `pub fn usage_page<'a>(items: impl IntoIterator<Item = &'a Entry>, history: &History, take: usize) -> Vec<Entry>`
  - 用过的（`rank_snapshot("").score(id) > 0.0`）按 score 降序在前，其余保持传入顺序；只 clone 前 `take` 条；`take == 0` 返回空。

- [ ] **Step 1: 写失败测试**

在 `lstr/src/search/rank.rs` 的 `mod tests` 末尾（`}` 之前）加入：

```rust
    #[test]
    fn usage_page_puts_used_first_and_keeps_natural_order() {
        let now = now();
        // Beta 用过 5 次、Gamma 1 次；Alpha 没用过。
        let history = history(&[("b", 5, now), ("c", 1, now)]);
        let entries = vec![
            entry("a", "Alpha", Kind::File),
            entry("b", "Beta", Kind::File),
            entry("c", "Gamma", Kind::File),
        ];

        let page = usage_page(entries.iter(), &history, 3);
        let names: Vec<&str> = page.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            ["Beta", "Gamma", "Alpha"],
            "用过的按 frecency 排前，其余保持自然序"
        );

        let page = usage_page(entries.iter(), &history, 2);
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].name, "Beta");
        assert_eq!(page[1].name, "Gamma");

        assert!(
            usage_page(entries.iter(), &history, 0).is_empty(),
            "take=0 必须返回空"
        );
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test usage_page_puts_used_first_and_keeps_natural_order`（工作目录 `lstr`）
Expected: 编译失败，`cannot find function 'usage_page'`（或 `not found in this scope`）。

- [ ] **Step 3: 实现**

在 `lstr/src/search/rank.rs` 的 `score_entry_planned` 结束后（`}` 与 `/// 盘符权重扣分` 之间）插入：

```rust
/// 类型页空输入的使用学习排序：用过的条目按 frecency 降序排前面，没用过的
/// 保持 `items` 给出的自然顺序（书签 = 名称升序，笔记 = 修改时间降序），只取
/// 前 `take` 条，且只 clone 这 `take` 条。
///
/// 必须在**全量候选**上调用：调用方不要把 `top(take)` 的结果传进来，否则名称
/// / 时间窗口之外的常用条目仍然升不上来（这正是本函数要修的问题）。
pub fn usage_page<'a>(
    items: impl IntoIterator<Item = &'a Entry>,
    history: &History,
    take: usize,
) -> Vec<Entry> {
    if take == 0 {
        return Vec::new();
    }
    let items: Vec<&Entry> = items.into_iter().collect();
    let snapshot = history.rank_snapshot("");
    let mut used: Vec<(f32, usize)> = items
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            let score = snapshot.score(&entry.id);
            (score > 0.0).then_some((score, index))
        })
        .collect();
    // 稳定排序：同分时保持自然顺序。
    used.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut out = Vec::with_capacity(take.min(items.len()));
    let mut taken = vec![false; items.len()];
    for (_, index) in used {
        if out.len() >= take {
            return out;
        }
        out.push(Entry::clone(items[index]));
        taken[index] = true;
    }
    for (index, entry) in items.iter().enumerate() {
        if out.len() >= take {
            break;
        }
        if !taken[index] {
            out.push(Entry::clone(entry));
        }
    }
    out
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test usage_page_puts_used_first_and_keeps_natural_order`
Expected: `test search::rank::tests::usage_page_puts_used_first_and_keeps_natural_order ... ok`，`test result: ok. 1 passed`。

- [ ] **Step 5: 提交**

```bash
git add src/search/rank.rs
git commit -m "feat(rank): 新增 usage_page，用过的条目按 frecency 排前"
```

---

### Task 2: 书签页空输入按使用历史排序

**Files:**
- Modify: `lstr/src/search/bookmarks.rs`（模块顶部 import；`top` 之后新增 `used_page`；`mod tests` 末尾加测试）
- Modify: `lstr/src/search.rs:2723-2731`（空查询书签分支）、`lstr/src/search.rs:2757-2759`（空文本 + 筛选分支）

**Interfaces:**
- Consumes: `rank::usage_page(items, &history, take) -> Vec<Entry>`（Task 1）
- Produces: `BookmarkIndex::used_page(&self, history: &History, take: usize) -> Vec<Entry>`

- [ ] **Step 1: 写失败测试**

在 `lstr/src/search/bookmarks.rs` 的 `mod tests` 末尾加入（`index_from` 与 `raw` 已在测试模块内定义）：

```rust
    #[test]
    fn used_page_lifts_bookmark_used_outside_the_name_window() {
        let index = index_from(vec![
            raw("Alpha", "https://a.example/"),
            raw("beta", "https://b.example/"),
            raw("gamma", "https://c.example/"),
        ]);
        let mut history = History::in_memory();
        let gamma = index
            .marks
            .iter()
            .find(|mark| mark.name == "gamma")
            .expect("gamma 书签应存在");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_secs();
        history.seed(gamma, 3, now);

        let page = index.used_page(&history, 2);
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].name, "gamma", "用过的书签必须排到名称窗口之前");
        assert_eq!(page[1].name, "Alpha", "没用过的仍按名称序");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test used_page_lifts_bookmark_used_outside_the_name_window`
Expected: 编译失败，`no method named 'used_page'` 且 `cannot find type 'History' in this scope`。

- [ ] **Step 3: 实现 + 接入**

3a. `lstr/src/search/bookmarks.rs` 顶部改 import：

```rust
use crate::history::History;
use crate::model::{Entry, Kind};
use crate::search::{apps, fuzzy, pinyin, rank};
```

3b. 在 `pub fn top` 之后插入：

```rust
    /// 书签页空输入的使用学习排序：用过的按 frecency 排前，其余保持名称序。
    /// 传全量索引（不要先 `top`），名称窗口外的常用书签才升得上来。
    pub fn used_page(&self, history: &History, take: usize) -> Vec<Entry> {
        rank::usage_page(self.marks.iter(), history, take)
    }
```

3c. `lstr/src/search.rs` 空查询书签分支整体替换为：

```rust
            } else if request.query.scope == Scope::Bookmark {
                // 书签页空输入 = 全部书签按「最近使用」排序（用过的在前，其余按
                // 名称），与应用页空输入的学习语义一致；取 deliver 而不是 limit，
                // 几百条书签也能滚到底「加载更多」。
                let take = request.deliver.max(request.limit);
                entries = if bookmarks_enabled.load(Ordering::Relaxed) {
                    scope_total = Some(bookmarks.count());
                    match history.lock() {
                        Ok(history) => bookmarks.used_page(&history, take),
                        Err(_) => bookmarks.top(take),
                    }
                } else {
                    Vec::new()
                };
            }
```

3d. `lstr/src/search.rs` 非空查询的 `Scope::Bookmark` 里，`request.query.text.is_empty()` 分支替换为（复用该 arm 顶部已定义的 `take`）：

```rust
                        if request.query.text.is_empty() {
                            total = bookmarks.count();
                            entries = match history.lock() {
                                Ok(history) => bookmarks.used_page(&history, take),
                                Err(_) => bookmarks.top(take),
                            };
                        }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test used_page_lifts_bookmark_used_outside_the_name_window`
Expected: PASS。再跑 `cargo test bookmarks::` 确认书签既有测试（含 `top_is_name_sorted_and_limited`）全绿。

- [ ] **Step 5: 提交**

```bash
git add src/search/bookmarks.rs src/search.rs
git commit -m "feat(bookmarks): 书签页空输入按使用历史排序"
```

---

### Task 3: 笔记页空输入按使用历史排序 + 搜索全量进 rank

**Files:**
- Modify: `lstr/src/search/notes.rs`（`top` 之后新增 `used_page`；`mod tests` 末尾加测试）
- Modify: `lstr/src/search.rs:1895-1921`（`notes_response` 空 / 非空分支）

**Interfaces:**
- Consumes: `rank::usage_page(...)`（Task 1）
- Produces: `NoteIndex::used_page(&self, history: &History, take: usize) -> Vec<Entry>`

- [ ] **Step 1: 写失败测试**

在 `lstr/src/search/notes.rs` 的 `mod tests` 末尾加入（`index` 与 `note_row` 已在测试模块内定义）：

```rust
    #[test]
    fn used_page_lifts_note_used_outside_the_modified_window() {
        // 自然序 = 修改时间降序：新笔记, 中间笔记, 旧笔记。
        let index = index(vec![
            note_row("新笔记", "", "1", 300),
            note_row("中间笔记", "", "1", 200),
            note_row("旧笔记", "", "1", 100),
        ]);
        let mut history = History::in_memory();
        let old = index
            .rows
            .iter()
            .find(|row| row.entry.name == "旧笔记")
            .expect("旧笔记应存在");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_secs();
        history.seed(&old.entry, 3, now);

        let page = index.used_page(&history, 2);
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].name, "旧笔记", "用过的笔记必须排到修改时间窗口之前");
        assert_eq!(page[1].name, "新笔记", "没用过的仍按修改时间序");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test used_page_lifts_note_used_outside_the_modified_window`
Expected: 编译失败，`no method named 'used_page'`。

- [ ] **Step 3: 实现 + 接入**

3a. 在 `lstr/src/search/notes.rs` 的 `pub fn top` 之后插入（`History` / `rank` 已在模块顶部导入）：

```rust
    /// 笔记页空输入的使用学习排序：用过的按 frecency 排前，其余保持修改时间序。
    /// 传全量索引（不要先 `top`），修改时间窗口外的常用笔记才升得上来。
    pub fn used_page(&self, history: &History, take: usize) -> Vec<Entry> {
        rank::usage_page(self.rows.iter().map(|row| &row.entry), history, take)
    }
```

3b. `lstr/src/search.rs` 的 `notes_response` 前半段替换为：

```rust
    let (entries, total) = if !enabled {
        (Vec::new(), 0)
    } else if request.query.text.trim().is_empty() {
        // 空输入 = 最近使用优先（frecency 排前，其余按修改时间）；
        // total 用真实总数，UI 才会发「加载更多」。
        let entries = match history.lock() {
            Ok(history) => notes.used_page(&history, take),
            Err(_) => notes.top(take),
        };
        (entries, notes.count())
    } else {
        let strict = !request.filters.matches.is_default();
        // 全量命中交给 rank（frecency / query_hits 才能把窗口外的常用笔记提上
        // 来），响应末尾再按 take 截断。严格匹配仍先取全量再复核。
        let limit = usize::MAX;
        let (mut hits, total) = match history.lock() {
            Ok(history) => notes.search(
                &request.query.text,
                limit,
                !strict,
                Some(&history),
                &request.rank,
            ),
            Err(_) => notes.search(&request.query.text, limit, !strict, None, &request.rank),
        };
        hits.retain(|entry| {
            app_matches_options(&entry.name, &request.query.text, &request.filters)
        });
        let total = if strict { hits.len() } else { total };
        hits.truncate(take);
        (hits, total)
    };
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test used_page_lifts_note_used_outside_the_modified_window`
Expected: PASS。再跑 `cargo test notes::` 确认笔记既有测试全绿。

- [ ] **Step 5: 提交**

```bash
git add src/search/notes.rs src/search.rs
git commit -m "feat(notes): 笔记页空输入按使用历史排序，搜索全量进 rank"
```

---

### Task 4: 删除应用 / 书签 / 「全部」页打分前的截断

**Files:**
- Modify: `lstr/src/search.rs:2736-2753`（`Scope::App`）
- Modify: `lstr/src/search.rs:2763-2773`（`Scope::Bookmark` 非空分支）
- Modify: `lstr/src/search.rs:2872-2945`（`Scope::All` 非文件来源）

**Interfaces:**
- Consumes: 既有 `rank::rank_with`（frecency + query_hits）与 `finalize` 末尾的 `entries.truncate(deliver)`；不新增接口。

- [ ] **Step 1: 删除应用页打分前截断**

`Scope::App` arm 替换为：

```rust
                Scope::App => {
                    // B2：应用页同样按 deliver 取数，滚到底「加载更多」才能真的
                    // 加载出后面的应用。P2-4：total 必须是全量匹配数；候选全量
                    // 进 finalize 打分，frecency / query_hits 才能把窗口外的常用
                    // 应用提上来，最终由 finalize 按 deliver 截断。
                    let matches: Vec<Entry> = apps
                        .search(&request.query.text, usize::MAX)
                        .into_iter()
                        .filter(|entry| {
                            app_matches_options(&entry.name, &request.query.text, &request.filters)
                        })
                        .collect();
                    total = matches.len();
                    entries = matches;
                }
```

- [ ] **Step 2: 删除书签页打分前截断**

`Scope::Bookmark` 非空分支里删掉 `hits.truncate(take);` 这一行（`total = hits.len();` 与 `entries = hits;` 保留）。

- [ ] **Step 3: 放开「全部」页的非文件来源**

3a. 把 `let source_take = request.deliver.max(request.limit);` 与 `let strict_matches = ...; let source_fetch = if strict_matches { usize::MAX } else { source_take };` 三行替换为：

```rust
                        // 非文件来源全部进 finalize 打分：应用/命令/书签/笔记总量
                        // 都小，打分前截断会让「用得多的冷门条目」永远升不上来。
                        let source_fetch = usize::MAX;
```

3b. 删除 `app_hits.truncate(source_take);`、`bookmark_hits.truncate(source_take);`、`note_hits.truncate(source_take);` 三行；其余（`total += ...`、`entries.append`）保持不变。

- [ ] **Step 4: 编译并跑全量测试**

Run: `cargo test`
Expected: 全部通过（无 `truncate`，无 unused 变量警告）。

- [ ] **Step 5: 手动冒烟（可选但推荐）**

Run: `cargo run`
检查：应用页输入关键词，同匹配档位里用过的应用靠前；书签页 / 笔记页滚动加载更多仍可用；「全部」页搜索后，用过的书签 / 笔记能出现在结果里。

- [ ] **Step 6: 提交**

```bash
git add src/search.rs
git commit -m "feat(search): 类型页候选全量进 rank，去掉打分前截断"
```

---

### Task 5: 文档与收尾

**Files:**
- Modify: `lstr/README.md:311`（浏览器收藏夹一行）
- Modify: `lstr/docs/superpowers/specs/2026-11-18-type-page-usage-ranking-design.md`（追加「实施记录」）

- [ ] **Step 1: 更新 README**

把 `lstr/README.md:311` 里的：

```
空输入按名称列出全部
```

替换为：

```
空输入按最近使用排序（用过的在前，其余按名称）
```

- [ ] **Step 2: 规格文档补「实施记录」**

在 `lstr/docs/superpowers/specs/2026-11-18-type-page-usage-ranking-design.md` 末尾追加：

```markdown
## 实施记录

- 2026-11-18：`rank::usage_page` + `BookmarkIndex::used_page` / `NoteIndex::used_page`；
  书签页、笔记页空输入改为使用历史排序；应用 / 书签 / 「全部」页非文件来源取消打分前截断；
  `notes_response` 非空分支改为全量进 rank。README 同步。
```

- [ ] **Step 3: 最终验证**

Run: `cargo test`
Expected: 全绿。

Run: `cargo build`
Expected: 编译成功，无警告。

- [ ] **Step 4: 提交**

```bash
git add README.md docs/superpowers/specs/2026-11-18-type-page-usage-ranking-design.md
git commit -m "docs: 补类型页使用学习排序的说明与实施记录"
```
