# 文件对话框停靠条设计（Listary 式吸附）

日期：2026-11-17
状态：已获用户批准（2026-11-17），待实施

## 背景与目标

现状：文件对话框一出现，「文件对话框增强」就把**整块启动器窗口**弹在对话框左上角（`position_near_dialog`），
并且抢焦点。用户视线要离开对话框、文件名框无法直接打字，观感上更像「又开了一个启动器」，而不是「吸附在对话框上的条子」。

用户参照 Listary 的交互：条子挂在**文件对话框下沿外侧、水平居中**，对话框自己仍持有焦点；条子里先显示
Quick Switch 建议，点条子或按 `Ctrl+G` 才把输入交给它。

目标：

1. 对话框出现时，条子被动停靠在对话框下沿外侧、水平居中，**不抢焦点**，对话框（含文件名框）照常可操作；
2. 点击条子或按热键才把焦点交给条子，此时可键入搜索、`↑↓` 选择、`Enter` 执行、`Esc` 收起；
3. 几何 / 跟随 / 尺寸槽位复用 Explorer 贴角浮层已有的引擎，不重复实现。

非目标（本轮不做）：

- 对话框内直接打字镜像到条子（用户已明确本轮不做）；
- 条内预览面板、筛选条、多栏布局；
- 条宽 / 位置的可配置项。

## 现状（代码事实）

- **Explorer 贴角浮层引擎**（工作区已实现，未提交）：
  - `src/app.rs` 字段 `attach_hwnd`、`attach_last`、`attach_px`、`attach_sel`、`attach_status_slot`、`attach_rows_floor`、`attach_focused`；
  - 常量 `ATTACH_W = 440`、`ATTACH_ROW_H = 32`、`ATTACH_INPUT_H = 40`、`ATTACH_MAX_ROWS = 9`、`ATTACH_DIVIDER = 1`、`ATTACH_MIN_H = 73`（`src/app.rs:306-334`）；
  - `place_attached` 用 `window::frame_bounds` 取锚点可见边框，一次 `window::set_rect` 同时改位置与尺寸（`src/app.rs:4194`）；
  - `poll_attach` 每帧轮询 + `request_repaint_after(16ms)`，锚点消失即 `hide_keep`（`src/app.rs:4230`）；
  - `paint_attach`：顶部按需状态行、中间结果列表（复用 `paint_rows`）、底部 1px 分隔线 + 40px 输入行贴底（`src/app.rs:6327`）。
- **对话框模式**（现状，将被替换表现层）：`open_dialog_mode` 设置 `dialog=Some(info)`、`scope=Folder`，再调 `window::position_near_dialog`（左对齐 + 56px 偏移，整块启动器窗口）。
- **对话框探测**：`src/win/dialog.rs` 的 `DialogInfo { hwnd, edit, is_save, vista }`、`active_dialog()`、`foreground_dialog()`；
  探测定时器 `DialogWatch`（`src/app.rs:1825`），`poll_dialog_watch`（`src/app.rs:2392`）在 `dialog_assist` 开启、启动器隐藏时触发，`suppressed_dialog` 抑制同一 hwnd 反复唤出。
- **执行语义**（保留）：`activate_index` 的 `dialog.is_some()` 分支——文件夹 `dialog::navigate_to_folder`、文件 `dialog::fill_path`、
  App 用 `shell::open_entry`；成功后 `suppressed_dialog=Some(hwnd)`、`dialog=None`、`hide_keep`、`dialog::restore_focus`（`src/app.rs:5102`）。

## 设计

### 1. 状态机

三个状态：

| 状态 | 窗口 | 焦点 | 输入行 | 结果区 |
|---|---|---|---|---|
| `Hidden` | 隐藏 | — | — | — |
| `Docked`（被动） | 显示 | 在文件对话框 | 占位提示「点击或 Ctrl+G 搜索」 | Quick Switch 建议（空查询） |
| `Engaged`（接管） | 显示 | 在条子 | 可编辑，光标在内 | 实时搜索结果 / 建议 |

转换：

| 事件 | 起始 | 结果 |
|---|---|---|
| 对话框成为前台、`dialog_assist` 开、该 hwnd 未被抑制 | Hidden | Docked |
| 点击条子任意位置 | Docked | Engaged |
| `Ctrl+G`（对话框在前台） | Hidden / Docked | Engaged（Hidden 时先摆位再接管） |
| 双击 `Ctrl`（对话框在前台） | Hidden / Docked | Engaged |
| `Esc` | Engaged | Hidden，焦点还给对话框，抑制该 hwnd |
| `Enter` 执行成功 | Engaged | Hidden，抑制该 hwnd，焦点还给对话框 |
| 焦点离开条子（用户点回对话框） | Engaged | Docked，清空查询 |
| 对话框关闭 / 隐藏 | 任意 | Hidden |
| 打开设置面板 | 任意 | Hidden |
| `dialog_assist` 关闭 | Docked | Hidden |

设计取舍：`Esc` / 执行后**整条收起**而不是常驻被动状态。理由：收起即「本轮完成」的明确信号，配合 `suppressed_dialog`
避免同一个对话框反复弹条子干扰输入；需要时 `Ctrl+G` 随时唤回。用户已确认该交互。

### 2. 几何

常量沿用 Explorer 浮层：宽 `ATTACH_W=440`、行高 `ATTACH_ROW_H=32`、输入行 `ATTACH_INPUT_H=40`、分隔线 1px、最多 9 行。

高度 `h = 40 + 1 + rows*32`，`rows` 默认 `min(结果数, 9)`，最少 1。

- **正常（下沿外侧）**：`x = dialog.center_x - w/2`（夹到工作区内），`y = dialog.bottom`（与对话框下沿齐平）。
- **下方空间不足**：先按 `available = workarea.bottom - y` 降 `rows`，直到 `h <= available` 或 `rows == 1`；
  连 `rows=1` 都放不下（`available < 73`）→ **内侧贴底**：`y = dialog.bottom - h`，条子压在对话框底部内侧。
- **水平夹取**：`x` 夹在 `[workarea.left + 4, workarea.right - w - 4]`。
- **跟随**：`frame_bounds(dialog_hwnd)` 每帧对比，变化即 `set_rect`；被动 50ms 轮询、接管 16ms。
- **对话框消失**：`window::is_window` 判定失败或不可见 → 立即隐藏。

几何计算抽成纯函数，便于单测：

```rust
fn bar_rect(dialog: [i32; 4], work: [i32; 4], rows: i32) -> (i32, i32, i32, i32) // x, y, w, h
```

### 3. 布局

自上而下：

1. **输入行**（40px）：左侧放大镜图标；中间 `TextEdit`（`id = "lstr-dialog-input"`）；右侧灰字提示——
   被动「点击或 Ctrl+G 搜索」，接管「Enter 跳转 · Esc 收起」。
2. **1px 分隔线**。
3. **结果行**（`paint_rows`，每行 32px）：
   - 空查询：Quick Switch 建议（Explorer 已打开的文件夹），行尾显示 `Ctrl+G` 意思的反向提示改显示盘符；
   - 有查询：实时搜索结果，命中高亮沿用 `paint_rows` 现有实现。

不画 Explorer 浮层那套「状态行」（选中项完整路径），紧凑条高度敏感；选中项的完整路径改为在输入行右侧灰字短暂显示。

### 4. 焦点与显示

- 被动出现必须**不激活窗口**：
  - `ShowWindow(hwnd, SW_SHOWNOACTIVATE)`；
  - `SetWindowPos(hwnd, HWND_TOPMOST, x, y, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW)`；
  - 不调用 `force_foreground` / `restore_foreground`。
- 接管时才 `restore_foreground(hwnd)` 并 `egui` 请求聚焦输入框。
- 风险：winit 侧的 `Visible(true)` 可能触发激活。实现时用 `GetForegroundWindow()` 断言：被动显示后前台仍是对话框 hwnd；
  若被激活，改走 `SW_SHOWNOACTIVATE` + 事后 `restore_foreground(dialog)` 兜底（并在手动清单里回归）。

### 5. 设置与默认

- `dialog_assist`（已有）：开 → 对话框出现自动 Docked；关 → 只有 `Ctrl+G` 能唤出（且直接 Engaged）。
- 不新增设置项。
- 与 Explorer 浮层互斥：两者同时只会有一个锚点，`attach_*` 状态由当前目标决定；对话框停靠期间 Explorer 浮层的 `poll_attach` 不运行。

### 6. 测试

- 纯函数单测 `bar_rect`：
  - 下方空间充足 → 下沿齐平、水平居中；
  - 下方空间只够 3 行 → rows 降为 3；
  - 下方空间不足 73px → 内侧贴底；
  - 对话框贴屏幕左缘 → x 被夹到 4；
  - 对话框高于工作区 → rows 至少 1、高度不超工作区。
- 手动清单：
  1. VS Code「打开文件」对话框 → 条子下沿居中、不抢焦点、文件名框可打字；
  2. 记事本「另存为」→ 同上；
  3. 拖动 / 缩放对话框 → 条子跟随无抖动；
  4. 点条子 → 接管，打字搜索，`↑↓` + `Enter` 文件夹跳转；
  5. `Esc` → 条子收起、焦点回对话框、同一对话框不再自动弹；
  6. 再按 `Ctrl+G` → 条子回来；
  7. 双击 `Ctrl` → 聚焦条子；
  8. 最大化 / 贴任务栏的对话框 → 内侧贴底兜底不越界；
  9. 对话框关闭 → 条子立刻消失；
  10. `cargo test` 全绿。

## 决策记录

- **方案 A（采用）**：新增「锚点目标」概念（Explorer / Dialog），复用现有几何、跟随、行数引擎；按目标切换摆放规则与焦点策略。
- 方案 B（另写一套对话框停靠逻辑）否决：跟随、尺寸槽位、抖动处理要复制一遍，两边会跑偏。
- 方案 C（只把大窗口挪到下方居中）否决：仍是启动器大窗，不满足「条子」观感。
