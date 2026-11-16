# 应用隐藏 / 恢复（v1）设计

日期：2026-11-16
状态：已实现（`master` 分支，未发布）

## 背景

应用页索引里有两百多个开始菜单 / App Paths 条目。多数用户日常只用其中十几个，
其余是「看得见但永远不点」的噪音；但又不希望永久删除——以后可能重新需要。

用户诉求（原话）：

> 有些我想要显示出来，然后后面可能以后要显示出来？这种逻辑和交互能做吗

追问后确定两点：

1. 需要**手动**把某个应用藏起来，需要时再放出来（不是自动学习、不是只有精选）。
2. 隐藏只在**空输入的应用页**生效；输入名字仍能搜到该应用，行尾标「已隐藏」，
   可直接取消隐藏——避免「藏完就再也找不到」。
3. 入口：Shift+F10 动作菜单（「隐藏此应用」/「取消隐藏此应用」）**加**
   应用页底部状态行提示与 Ctrl+H 临时显示。

## 存储

`config.toml` 新增表：

```toml
[[hidden_apps]]
name = "Clash Verge"
path = "C:\\ProgramData\\Microsoft\\Windows\\Start Menu\\Programs\\Clash Verge.lnk"
```

- `#[serde(default)]`：老配置没有这段照常解析。
- 路径比较一律 `eq_ignore_ascii_case`（Windows 盘符 / 目录大小写不敏感）。
- `name` 只为人工阅读（用户直接改配置时看得懂），匹配只用 `path`。
- 每次切换立即 `cfg.save()`（原子写，沿用既有实现）。

备选方案与否决理由：

| 方案 | 结论 |
| --- | --- |
| 存 `history.db` | 否：清历史就丢、用户无法手改，且历史库是可丢弃缓存 |
| 规则式隐藏（`name:微信*`） | 否：对「只藏几个」的目标过重，还要额外的规则语法与优先级 |
| 写入 `config.toml` 列表 | **采用**：可读、可手改、与收藏/动作/命令同一套资产 |

## 行为

| 场景 | 行为 |
| --- | --- |
| 应用页空输入 | 隐藏项不出现（收藏置顶项若被隐藏也一起藏），底部显示「已隐藏 N 个 · Ctrl+H 显示」 |
| 搜索（任意范围） | 隐藏项照常出现，副标题前缀「已隐藏」，Enter 仍能启动（不自动恢复） |
| Ctrl+H | 应用页内切换：隐藏项重新出现并排在最前，底部变「已隐藏 N 个 · Ctrl+H 收起」 |
| 动作菜单 | 应用条目多一项「隐藏此应用」/「取消隐藏此应用」，就地增删并 toast |
| 重新加载配置（命令页 `reload`） | 隐藏列表重新下发给搜索 worker |

关键取舍：

- **隐藏优先于收藏**：收藏只是置顶，不代表不可隐藏；否则「★ 微信」永远删不掉。
- **总数按过滤后计算**：`total` 用过滤后的真实条数，滚动「加载更多」语义不变。
- **不改 `Entry`**：标记与 ★ 一样由 UI 每帧从 `cfg` 建集合，worker 协议只多两个字段
  （`hidden: Vec<String>` 小写路径、`show_hidden: bool`）。

## 实现位置

| 文件 | 内容 |
| --- | --- |
| `src/config.rs` | `struct HiddenApp { name, path }`；`Config.hidden_apps`（`serde(default)`） |
| `src/assets.rs` | `hide_app` / `unhide_app`（落盘）、`hide_app_in_memory` / `unhide_app_in_memory`（测试用）、`is_hidden` |
| `src/search.rs` | `Request.hidden` / `Request.show_hidden`；`SearchService::set_hidden` / `set_show_hidden`；`app_scope_entries` 过滤与置顶排序 |
| `src/app.rs` | `LstrApp.show_hidden`；Ctrl+H；`MenuAction::ToggleHide`；`menu_items_with(..., hidden)`；`entry_hidden`；行内「已隐藏」标记；状态行与快捷键提示 |

## 测试

新增 7 个用例（合计 439 passed）：

- `search::apps::tests::push_app_merges_same_app_with_different_separators`
- `search::apps::tests::push_app_keeps_prettier_name_when_it_comes_first`
- `search::apps::tests::name_key_folds_case_and_separators_only`
- `search::tests::app_scope_filters_hidden_apps_unless_revealed`
- `search::tests::app_scope_revealed_hidden_rows_come_first`
- `assets::tests::hide_and_unhide_are_case_insensitive_and_idempotent`
- `assets::tests::hidden_apps_survive_config_round_trip`

`cargo clippy --all-targets` 对改动文件零新增警告（其余为既有警告）。

## 附带修复：同一应用出现两行

搜索 `c` 时会出现「Clash Verge」与「clash-verge」两行。根因是应用索引
`push_app` 只按 `name.to_lowercase()` 去重，而安装程序同时往**用户级**
`%APPDATA%\...\Start Menu\Programs` 与**机器级** `%PROGRAMDATA%\...\Start Menu\Programs`
各写一份 `.lnk`，名字只差大小写 / 空格 / 连字符。

修复：去重键改为 `name_key`（忽略大小写、空白与 `-` `_` `.` `·`），重名时用
`display_score`（带空格 +2、带大写 +1）保留更「像产品名」的那份。
实测本机有 8 组这样的重复（Clash Verge、FileZilla、VS Code、微信开发者工具、
腾讯会议等），全部是同一应用的两份快捷方式，合并安全。

不采用的方案：解析 `.lnk` 目标做真去重——需要 COM / `IShellLink` + unsafe，
226 个链接的启动开销不值得；代价是「微信」与「WeChat」这类异名同目标仍可能各占一行。

## 不做（v1）

- 设置页的「已隐藏」列表编辑（当前只能通过动作菜单或手改 `config.toml`）。
- 隐藏整个文件夹 / 文件 / 书签（只针对应用）。
- 隐藏的自动过期或「最近藏过」推荐。
