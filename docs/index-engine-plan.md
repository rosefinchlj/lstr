# lstr 自研索引引擎:可行性与实施方案

> 2026-11-08 · v2（评审修正版）· 目标:摆脱 Everything 外部依赖,自建文件名索引,达到同级体验。
> v1 基于估算;v2 把关键数字换成本机实测,修正 3 处与技术事实不符的设计,并补渐进启动 UX。
> 仅 P0 各项性能仍为待测估算,文中以「估」标注。
>
> **2026-11-12 更新：Everything 后端已从程序中移除，只保留自研索引。**
> 本文中「Everything 兜底 / 双后端 / Fallback 走 Everything」等段落仅作历史设计记录。
> 同日已落地 size 索引列（`VolumeStore.size`、快照 v6、USN/mtime 合并回填），
> 文中「不存 size/mtime 列」的取舍与内存估算已过时。

## 0. 结论（TL;DR）

**可行,且值得做。** Windows 上唯一能达到 Everything 同级的自建路线是
**NTFS USN 索引**,Rust 侧只需现有 windows-rs 依赖,无第三方硬依赖。

四条决定性事实（v2 修正）:

1. **需要管理员权限**——读卷/USN 必须提权,且无提权的
   `FSCTL_READ_UNPRIVILEGED_USN_JOURNAL` **不返回文件名**,无法用于名字索引。
   故必须有独立的提权数据泵进程,主进程保持非提权。
2. **仅 NTFS**——本机 4 个卷全部 NTFS(C/D/E/F),v1 不存在覆盖缺口;exFAT 卷走 browse/Everything 兜底。
3. **必须排除噪声目录**——本机索引项 845 万,其中噪声类占 53%（`node_modules` 单项 349 万）。
   不排除则内存 ~335MB,排除后 ~170MB。默认排除,可关。
4. **渐进启动**——快照 mmap 加载近乎零等待;只有首次安装/journal 回绕才需后台全量扫描,
   期间界面照常可用,状态行显示进度,空查询走历史、双击 Space 走实时 Explorer 枚举。

**阶段**:P0 spike 半天~1 天定 go/no-go → P1 引擎核(~2k LOC) → P2 数据泵(~800) → P3 接入(~1k)。
Everything IPC 保留为自动兜底,不做一刀切替换。

---

## 1. 现状与动机

### 现状基线（代码事实）

- `search/mod.rs` 的 `run_worker` 是编排中枢:`EverythingProvider`(文件)+
  `AppIndex`(开始菜单/App Paths)+ UWP(懒加载)+ `browse::DirCache`(路径模式)
  + 命令/关键字行,产出 `Vec<Entry>` 进 `finalize_entries`(rank → 去重 → 截断 → 置顶)。
- Everything 侧补偿逻辑:文件夹/文件**双路取候选**、`wfn:` **精确同名兜底**、
  `f ` 前缀限文件。三者都是「外部引擎会截断」的产物,自建后应当**删除**而不是保留。
- 拼音(fuzzy/pinyin)、打分(rank)、噪声规则都在本地 —— Everything 只负责取候选。
- 测试基线 **150** 个 `#[test]`（v1 文档写的 142 已过期）。

### 自建的动机

| 痛点（均为既有实测） | 自建后 |
|---|---|
| 查询词顺序影响 8 倍延迟 | 候选生成本地,语义可控 |
| 按修改时间排序比按名慢一倍 | 索引内选列,无外部排序策略 |
| 依赖外部进程（未装/未运行要引导） | 零依赖发行;Everything 降级为可选加速 |
| 精确同名要 `wfn:` 二次查询补偿 | 索引内二分,天然支持 |
| 预筛在外部、打分在本地,语义割裂 | 预筛+打分同进程,拼音/密度门槛全局一致 |
| 路线图 P3「大结果窗口」「查询历史」受制于人 | 奠基 |

### 不取的替代方案

- **全盘目录遍历**（ReadDirectoryChangesW）:分钟级冷启动、漏受保护目录,严格劣于现用 IPC——只配做非 NTFS 兜底。
- **Windows Search(COM)**:延迟不可控、覆盖不齐、API 深坑。
- **手写 MFT 扇区解析**:USN 已给出 FRN/父 FRN/名字/属性,不必造这个轮子。
  （注意:这不是「USN 什么都有」——见 §3.2 能力边界。）

---

## 2. 本机实测基线（2026-11-08）

| 指标 | 实测值 | 说明 |
|---|---|---|
| 卷 | C/D/E/F 全 NTFS | C 643GB、D 620GB、E 404GB、F 356GB |
| 索引项总数 | **8,450,170** | C 2,968,922 / D 3,040,285 / E 2,424,802 / F 22,161 |
| 平均文件名 | 10.9 字符 / **11.0 UTF-8 字节** / 最大 67 字节 | 抽样 2 万条 |
| 非 ASCII 名占比 | **0.2%** | 拼音附加存储可忽略（<1MB） |
| 噪声路径占比 | **53%**（4.76M，含重叠） | `node_modules` 3.49M、Temp 0.65M、WinSxS 0.31M、.rustup 0.14M、.git/objects 0.085M |
| Everything 常驻内存 | 722MB private / 784MB WS | 845 万项 ≈ 85B/项（含全路径缓存、size/date 列、运行计数） |
| C: USN 日志 | 上限 **599MB**、已用 75MB | journal 回绕风险低于 v1 假设 |
| 支持的记录版本 | 2 ~ 4 | 解析需按 MajorVersion 分派 |

**结论**:方案 v1 的「200 万文件 / 150~250MB」在数量级上错了 4 倍。
修正后按 §3.3 的紧凑布局,845 万项约 335MB、排除噪声后 380 万项约 170MB。

---

## 3. 技术路线与能力边界

### 3.1 两个核心 API

```
FSCTL_ENUM_USN_DATA        全卷文件记录流式枚举（可传 StartFileReferenceNumber 断点续扫）
                           USN_RECORD_V2:FRN、父 FRN、USN、记录时间戳、属性、名字
FSCTL_READ_USN_JOURNAL     按 USN 位点增量读变更；阻塞等待模式实现准实时（<100ms）
FSCTL_GET_NTFS_VOLUME_DATA 取 MftValidDataLength / BytesPerFileRecordSegment
                           → 扫描进度百分比的分母（提权后一次调用，成本可忽略）
```

均走 `DeviceIoControl`;windows-rs 需追加 `Win32_System_Ioctl`
(`Win32_Storage_FileSystem`、`Win32_Security` 已在 Cargo.toml 中)。

### 3.2 能力边界（v1 方案的最大事实错误）

**USN 记录不含文件 size,也不含文件修改时间。** `TimeStamp` 字段是日志事件时间,
不是 `$STANDARD_INFORMATION` 里的 mtime。因此:

| 现有功能 | 依赖 | USN 能直接给 | 结论 |
|---|---|---|---|
| 时间筛选 `TimeFilter::Today/Week/Month/Year`(`src/app.rs:1591`) | mtime | ✗ | 需补齐或降级 |
| `recency_points`(`src/search/rank.rs:113`,≤15 分) | mtime | ✗ | 静默归零,排序漂移 |
| size 展示 | size | ✗ | 需补齐 |
| 精确/前缀/子串/拼音匹配 | name + attrs | ✓ | 直接可用 |

**mtime/size 三条路线:**

- **(A) 懒填（v1 默认）** 结果定稿后按**父目录分组**做 `FindFirstFileEx`,一次拿到该目录全部兄弟的
  size/mtime/精确名字。结果集通常只涉及 5~20 个父目录,成本 ≈1~5ms,只作用于最终 ≤100 条。
  覆盖:size 展示、`recency_points`(在打分前填即可保留)、显示用精确大小写。
  **不覆盖**:时间筛选需要全体匹配项的 mtime。
- **(B) 时间筛选降级（v1 默认，与 A 配套）** 时间筛选按钮存在时:有 Everything → 该次查询转由
  Everything 执行;无 Everything → chip 置灰 + tooltip「本地索引不含文件时间」。
- **(C) 裸 MFT 解析（v3 备选）** 解析 `$MFT` / `$STANDARD_INFORMATION` 拿全量 size/mtime,
  即 Everything 的真实做法。**+1~2k LOC**,含 fixup、属性表、数据运行解析,风险显著。
  只有「时间筛选/日期排序必须同级」时才上。

> 决策:v1 = A + B。P3 验收里「全功能等价」改为「除时间筛选外功能等价」,时间筛选走 B。

### 3.3 其它关键行为事实

- **目录改名不级联。** NTFS 改目录名只为**目录本身**发一条 rename 记录,子孙不重发
  （子孙的父 FRN 与名字都没变）。所以索引**必须**自己做父链路径解析——这也正是本设计方案的选择,
  且目录改名在索引里是**零成本**的（不需要动任何子孙）。
- **USN 与 ACL 无关**:卷句柄上的记录级读取不受目录 ACL 限制,名字全覆盖。
- **journal 会被回收**:`NextUsn` 校验失败或 journal reset → 该卷全量重扫自愈（卷级）。
  本机 C: 上限 599MB,回绕窗口很大,风险等级由「中」降为「低」。
- **卷到达/移除**:`WM_DEVICECHANGE` 触发该卷重扫/摘除。
- **记录版本**:按 `MajorVersion` 分派 V2/V3/V4,未知版本整批丢弃并计入告警计数。
- **幽灵与元数据项**:`ENUM_USN_DATA` 可能返回已删记录的残留名字、以及 `$MFT`/`$Extend` 等
  元数据项。按 FRN/属性/名字前缀过滤 —— 列入 P0 验证项。

---

## 4. 架构设计

### 4.1 进程模型

**主进程提权不可取**:启动器以管理员运行 → 拉起的子进程（记事本、终端…）全部继承管理员权限,
拖拽/IME/安全面全坏。**Everything 也不用这条路。**

```
┌─ lstr.exe(非提权,UI,热键 t=0 可用)───────────────┐
│  BuiltinProvider → 内存索引库(存储/查询/打分)      │
│    · 快照 mmap(冷启动零等待)                      │
│    · 状态机:Loading/Replay/Scanning/Ready/Fallback │
│  ↑ 命名管道 \\.\pipe\lstr-index                    │
└──┬─────────────────────────────────────────────────┘
   │ 握手 {volumes, journal_id, next_usn} → 记录流
┌─ lstr.exe --index-worker(提权,无窗口,常驻)──────┐
│  ENUM_USN_DATA(可断点续扫)/ READ_USN_JOURNAL      │
│  GET_NTFS_VOLUME_DATA(进度分母)                    │
│  只读转发,不持有索引、不落盘                        │
└────────────────────────────────────────────────────┘
```

- **握手协议（v1 修正）**:主进程连接时下发每卷 `{journal_id, next_usn}`（从快照头读）。
  worker 校验 journal 未回绕则**只推增量**;不符才全量重扫。
  **不是**「无状态所以全量重推」——那会让每次 lstr 重启都抖 20~40s 磁盘。
- **提权方式**:v1 `ShellExecute("runas")` 拉起,拒绝 → 自动回落 Everything;
  设置页一键安装计划任务（`RunLevel Highest`,登录自启）作为默认推荐路径,免后续 UAC。
- **管道安全（v1 必做）**:worker 是提权进程,管道若不设 ACL,同机任意账户可连接并拿到全盘文件清单。
  措施:建管道时指定 `SECURITY_ATTRIBUTES`（仅当前用户 SID）+
  `GetNamedPipeClientProcessId` 校验对端为本用户 lstr.exe。
- **查询零 IPC**:索引与查询都在主进程,击键路径无跨进程开销。

### 4.2 存储结构（紧凑版，取代 v1 的 UTF-16 + u64 布局）

```
names      UTF-8 精确名字,紧凑大 buffer               11.0B/项
names_lc   UTF-8 小写折叠副本,仅供匹配               11.0B/项
offs       排序后的 u32 偏移（按 names_lc 小写序）     4B/项  → 前缀=二分
frn        u64 FileReferenceNumber 有序数组            8B/项  → 增量定位(批量归并)
parent     u32 父目录槽位（不是 u64 FRN）              4B/项  → 路径沿父链解析
attrs      u16 属性位(目录/系统/隐藏…)                2B/项
pinyin     稀疏表,仅非 ASCII 名（0.2%）               ~1MB
```

| 规模 | 名字×2 | offs | frn | parent | attrs | 合计 |
|---|---|---|---|---|---|---|
| 845 万（全量） | 186MB | 34MB | 68MB | 34MB | 17MB | **≈335MB** |
| 380 万（默认排除噪声） | 84MB | 15MB | 30MB | 15MB | 8MB | **≈170MB** |

对比锚点:Everything 845 万项 722MB（它还存全路径缓存与 size/date 列）。

- **为什么 UTF-8 而非 v1 的 UTF-16**:名长 11 字节、99.8% ASCII,UTF-16 名字区要 184MB
  （UTF-8 只 93MB);更重要的是子串扫描 —— UTF-8 可直接用 `memchr::memmem` 的 SIMD 路径,
  UTF-16 只能 2 字节步进比较,慢 5~10 倍。**这是内存与延迟双赢的改动。**
- **为什么两份名字**:折叠副本让子串扫描无需大小写分支;精确副本保证显示与「复制路径」正确。
  若内存成为硬约束,v2 可去掉精确副本、改由父目录枚举回填显示名（复用 3.2 的 A 步）——预计再省 ~90MB,留作后手。
- **不存 size/mtime 列**:USN 拿不到（§3.2),按需填充,避免 135MB 空转。
- **前缀查询**:对 `offs` 二分,<1ms。
- **子串查询**:对 `names_lc` 全量扫描,845 万 ≈ 93MB,`memchr` SIMD 估 10~30ms;
  默认配置(380 万)≈ 5~15ms。**必须实测**,列入 P0 门槛。
- **模糊/拼音全拼**:只对候选集现算现有 `fuzzy::score_dense` / `pinyin::full`,不预存。
- **trigram 倒排**:v2 杠杆,内存 +30~50%,**不先建**。

### 4.3 候选选择策略（v1 未定义，必踩）

Everything 靠「日期排序截断 + `wfn` 补偿」决定哪 500 条进打分器;自建后必须显式定义,
否则 `a` 这类查询命中百万级时结果不可预测。v1 规则:

1. **精确同名**:二分全量（条数少,全给）。
2. **前缀命中**:二分区间全量（受上限保护）。
3. **子串命中**:按 `names_lc` 顺序扫描,封顶 `candidate_cap`(默认 2000) 后停止。
   落在名字序前段,结果稳定可复现;`total` 仍在扫描中精确统计（只计数不收集）。
4. 上述合并后按 `finalize_entries` 打分、去重、截断到 100。
5. `search_exact` / `search_split` 双路补偿**删除**——不再需要两阶段返回。

### 4.4 快照与加载

- 路径:`%APPDATA%\lstr\index\<volume-guid>.bin`（**不用盘符**,U 盘盘符漂移会串卷）。
- 头部:magic、版本、卷 GUID/序列号、JournalID、NextUsn、条数、各列偏移、扫描断点 Flrn。
- 载入:**mmap**,启动零等待;首查询可能因缺页慢 100~300ms,后台按顺序低优先级 prefetch 预热。
- 写盘:在搜索 worker 线程内全量重写（170MB ≈ 0.1~0.3s),UI 与退出都不阻塞;
  触发条件 = 退出时 best-effort + 脏变更累计超阈值且空闲时。
- **排序顺序必须持久化**:380 万条按小写序排序,串行比较 3~5s;每次启动重建不可接受。
  快照存已排序的 `offs`/`frn`,增量变更走「攒批 → 排序 → O(n) 归并」。

---

## 5. 渐进启动与索引状态 UX（新增）

目标:**进程与界面永不等待索引**。冷启动慢只体现在「结果新鲜度」,不体现在「能不能用」。

### 5.1 状态机（每卷独立 + 一个聚合态）

| 状态 | 触发 | 界面文案 | 可搜范围 |
|---|---|---|---|
| `Warm` | 有有效快照,mmap 中 | 不显示 | 快照内容（首查可能慢 100~300ms） |
| `Replay` | 快照之后的增量重放 | `正在同步变更…`（>300ms 才显示） | 快照内容 |
| `Scanning` | 无快照 / journal 回绕 | `正在构建索引 37% · 已收录 312 万项` | 已扫到的部分 |
| `Ready` | 扫描/重放完成 | 不显示 | 全量 |
| `Fallback` | 提权被拒 / worker 未运行 | `本地索引未启用 · 正在使用 Everything` | Everything |
| `Unavailable` | 两者皆无 | `本地索引未启用:需要管理员权限`（可点击进设置） | 仅历史/App/命令 |

### 5.2 交互规则

1. **t=0 可用**:空查询 = 最近使用（SQLite 历史,`history.rs`),完全不依赖索引 —— 装好即用。
2. **构建期可搜**:`Scanning` 时查询走「已加载部分」,结果照常排名展示;有 Everything 时合并去重
   （仅构建期付出这份 ~120ms,`Ready` 后不再并）。
3. **双击 Space（对话框模式）立即可用**:已打开的 Explorer 文件夹来自
   `win::explorer::open_explorer_folders()` 的**实时 Win32 枚举**,不依赖索引;构建期照常置顶。
4. **文案不骗人**:不显示假进度。百分比分母来自 `GET_NTFS_VOLUME_DATA` 的
   `MftValidDataLength / BytesPerFileRecordSegment`;`Replay` 用 `(cur-start)/(next-start)`。
5. **不闪**:状态行仅在「非 Ready 且持续 >300ms」时出现;转 `Ready` 立即消失。
6. **不新增浮层**:复用现有错误 banner 的渲染与高度计算（`app.rs:1252-1261` 的绘制、
   `app.rs:537/986/1236` 的 `banner_h` 三处高度),文案区改为「错误 or 索引状态」二选一。
7. **重绘节流**:扫描线程按 4Hz 发状态 + `ctx.request_repaint()`,不得逐条重绘。
8. **卷级状态**:插 U 盘只说该卷（`正在索引 F: 4%`),不重启全局状态行。
9. **可恢复**:`ENUM_USN_DATA` 断点（`StartFileReferenceNumber`）随快照持久化 ——
   扫描途中退出,下次从断点继续,不从头再来。

---

## 6. 查询管线接入（接缝与工作量）

| 接缝 | 位置 | 改动 |
|---|---|---|
| 编排中枢 | `src/search/mod.rs:230` `run_worker` | provider 抽象成 `enum Backend`,加索引状态通道 |
| 响应结构 | `src/search/mod.rs:43` `Response` | 加 `Status`（或独立 `status_rx`） |
| provider 签名 | `src/search/everything.rs:21/34/60` | Builtin 对齐 `search`;`search_split`/`search_exact` 走默认空实现 |
| 筛选映射 | `src/search/filters.rs:8` `SearchFilters` | 位置→父链前缀、类型→扩展名、时间→降级策略 B |
| 打分链 | `src/search/rank.rs:113` | 打分前懒填 mtime 以保留 `recency_points` |
| Entry 字段 | `src/model.rs:74` | `kind/size/modified/tone/initial/pinyin` 均需 provider 提供 |
| 共用工具 | `src/search/everything.rs:235-249` | `kind_of`/`tone_for`/`initial_of` 抽到公共模块再复用 |
| 历史过滤 | `src/search/mod.rs:554` `stale_history_entry` | 不变（仍按路径存在性） |
| 时间筛选 UI | `src/app.rs:1591` | chip 置灰 + tooltip（策略 B） |
| 噪声规则 | `src/config.rs:97` `default_noise_rules` | 直接复用为索引排除表 |
| 启动自启 | `src/win/autostart.rs` | 涉及登录时 UAC 时机（见 §8） |
| 构建 | `Cargo.toml`（当前**无** `[workspace]`） | 引入 `lstr-index` 需改 Cargo.toml + Cargo.lock |

**工作量修正**:v1 说「接线 ~30 行」偏乐观。Entry 构造要沿父链重建全路径、
kind 分类、scope/筛选映射、两阶段返回拆除、状态通道 —— 实际 **250~400 行**。
总量:P1 引擎核 ~2k + P2 数据泵 ~800 + P3 接入 ~400 ≈ **3.2~4k LOC**（与 v1 估计一致）。

**与 workbuddy 的冲突面**:新 crate 隔离了引擎代码,但 `Cargo.toml`/`Cargo.lock`
是双方都会动的文件 —— 方案 v1 说「零冲突」不准确。开工前 rebase,并把 workspace 改造做成第一个独立提交。

---

## 7. 分阶段实施与验收门槛

### P0 — Spike（独立 `probe-usn/`,0.5~1 天,零冲突）

仿 `probe-everything` 先例（在 lstr 仓库外）:全卷枚举 + journal 增量 + 计时/内存 + 子串微基准。

**门槛（本机实测,分两组）**:

| 指标 | 全量 845 万 | 默认（排除噪声）380 万 |
|---|---|---|
| 枚举耗时 | ≤60s | ≤30s |
| RSS（索引就绪） | ≤350MB | ≤200MB |
| 扫描峰值 RSS | ≤450MB | ≤280MB |
| 子串 p95（UTF-8 SIMD） | ≤30ms | ≤15ms |
| 快照大小 / 加载 | ≤350MB / mmap 即时 | ≤180MB / mmap 即时 |
| 排序（冷建） | ≤2s（并行） | ≤2s |

外加验证 4 项:① 父目录分组填 size/mtime 实测耗时;② `ENUM_USN_DATA` 是否吐出已删/元数据幽灵记录;
③ 改目录名真实记录条数（`fsutil usn readjournal` 前后对比,预期 1 条）;④ UTF-8 vs UTF-16 子串扫描对照。

不达标 → 回炉（调布局/上 trigram）;仍不达标 → 放弃,Everything 续用,损失半天。

### P1 — 引擎核（~2k LOC,新 crate `lstr-index`）

store（插入/改名/删除/前缀/子串/精确/父链路径）、USN 解析（V2/V3/V4 分派、8 字节对齐
`read_unaligned`、幽灵过滤）、小写折叠与拼音、快照读写（mmap + 排序持久化）。
测试:合成 USN 操作序列 vs `BTreeMap` 暴力模型的属性测试;快照 roundtrip;续接现有 150 测试风格。

### P2 — 数据泵 worker + 管道（~800 LOC）

`lstr.exe --index-worker`、runas 拉起与重连、握手 `{journal_id, next_usn}`、
长度前缀帧协议、全量重推 + 增量批次、断点续扫、卷热插拔、journal reset 自愈、**管道 ACL 与对端校验**。
测试:进程内模拟管道对端;VHD(format NTFS) 集成测试默认 `#[ignore]`。

### P3 — 接入 lstr（~400 LOC + UX）

`BuiltinProvider` 接线、`[index]` 配置、状态机与状态行 UI（复用 banner）、设置页
（状态/卷清单/重建/包含噪声开关/免 UAC 安装）、Everything 自动兜底。
**验收**:无 Everything 环境下 —— 除时间筛选外功能等价;首次安装 20~40s 构建期内界面全程可用、
可搜索、双击 Space 有实时结果、状态行进度真实。

### P4 — 打磨

大结果窗口（路线图 P3）、内存优化（去精确名字副本）、按需 trigram、
计划任务默认化、（可选）裸 MFT 升级到真正同级。

---

## 8. 风险与对策（修正）

| 风险 | 等级 | 对策 |
|---|---|---|
| 内存超预算 | 中 | 默认排除噪声（已实测省 53%）、紧凑列、P0 实测定红线 |
| 首装/journal 回绕后 20~40s 构建 | 低（有 UX 兜底） | 渐进启动 + 部分可搜 + 真实进度 + 断点续扫 |
| 免 UAC 前每次登录弹一次 | 中 | 默认推荐装计划任务;拒绝则回落 Everything 不劣化 |
| 管道被同机他用户连接 | 中 | pipe ACL + 对端 PID/SID 校验（v1 必做） |
| 时间筛选能力缺口 | 中 | 策略 B:转 Everything / 置灰提示;真同级留给裸 MFT（v3） |
| journal 被其他软件打爆 | 低 | 本机上限 599MB;卷级重扫 + 节流 |
| 与 Everything 排序微差 | 低 | 接受;同分按名序,`recency` 由懒填 mtime 保住 |
| workbuddy 并行冲突 | 中 | Cargo.toml/lock 是共同触点:workspace 改造单独先提交,开工前 rebase |
| ReFS/exFAT 覆盖缺失 | 低 | 本机全 NTFS;设置页明示,走 browse/Everything 兜底 |

## 9. 决策记录

**已定（本次评审拍板,可后续推翻）**

1. 默认后端 `auto`:worker 可用 → 内置;否则 Everything;都没有 → 提示。
2. 内存红线:全量 ≤350MB,默认配置（排除噪声）≤200MB。
3. 名字存储改 **UTF-8 精确 + 小写折叠副本**（不是 v1 的 UTF-16）。
4. mtime/size 走 **策略 A 懒填 + 策略 B 时间筛选降级**;裸 MFT 留 v3。
5. **默认排除噪声目录**（复用 `default_noise_rules`）,设置页可关。
6. **渐进启动**:进程 t=0 可用;构建期状态行显示真实进度;空查询走历史、双击 Space 走实时枚举。
7. worker 握手带 `next_usn`,增量续接;不做「每次重连全量重推」。
8. 快照按 **volume GUID** 命名,不按盘符。

**待定**

1. 免 UAC 是「默认引导安装计划任务」还是「v1 先 runas,v2 再引导」。
2. v3 是否要为时间筛选/日期排序上裸 MFT（+1~2k LOC,风险高）。
3. 是否提供「索引根目录白名单」（只索引指定目录可再省内存,但偏离 Everything 语义）。


## 10. 卷选择:多盘纳入 / 单盘取消（2026-11-09 落地）

**背景**:首版 `--index-volumes` 只是启动参数的 allowlist,验收时用 `volumes=["F"]`
跑完没还原,导致「只索引了 F 盘」。用户诉求:(1) 盘符按 C D E F 排序显示;
(2) 能选中/取消某个盘。

**配置**:`[index] volumes = []`(字符串数组,大写盘符)。语义保持向后兼容:
- 空数组 = **全部本机固定 NTFS 卷**(新插盘自动纳入,UI 显示「全部」);
- 非空 = 显式 allowlist,升序落盘。`Config::load` 会 `normalize_volumes()`
  (大写 / 去重 / 升序)。

**位掩码**:`lstr-index::volmask`(bit i = 盘符 `'A' + i`,`MASK_ALL = u32::MAX`)
是所有层之间的唯一表示:配置 ↔ 引擎 ↔ worker ↔ UI。空配置折算成 `MASK_ALL`。

**四层落点**:

| 层 | 行为 |
|---|---|
| 配置 → 主进程 | `IndexConfig::{volume_letters,volume_mask,volume_enabled,set_selected_volumes}`;`SearchService::set_index_volumes(mask)` 更新共享 `Arc<AtomicU32>` 并投递 `ClientCommand::SetVolumes` |
| 主进程 → worker | 协议 v5 新增 C→W `Frame::SetVolumes{mask}`(帧号 17)。worker 存进 `RUNTIME_VOLUME_MASK`,**不自己起停线程**,交给既有的 500ms 卷热插拔差异逻辑按新过滤结果补发 `VolumeAppeared`/`VolumeGone` |
| worker 拉起参数 | `--index-volumes C,D` 按**当前掩码**现算(掩码=全部时不传,让 worker 自己枚举);worker 重启也跟随最新选择 |
| 引擎 | `EngineConfig.volumes`:启动加载 `vol_*.bin` 时跳过未选中的卷(快照文件保留,重新勾选后下次启动直接复用);`status()` 出口按 letter 升序(C D E F)输出 |

**运行期语义**:取消某盘 → 一个 500ms 周期内 worker 停掉该卷线程并发
`VolumeGone`;引擎删掉该卷内存态与快照(不占内存)。重新勾选 → `VolumeAppeared`
+ 全量 dir 扫描(该盘无快照,从头建)。**不需要重启 lstr,也不会重弹 UAC**。
索引后端 / 噪声目录开关仍需重启。

**UI**:设置 → 搜索 → 索引卷,一行 chips(`C:` `D:` `E:` `F:`,升序,选中 amber)。
候选盘 = 本机固定 NTFS 盘 ∪ 配置里显式列出的盘 ∪ 当前引擎状态里的盘。
最后一个盘不允许取消(至少保留一个);勾满所有已知盘会折叠回「全部」(空数组)。

**测试**:`volmask` 位运算、`IndexConfig` 归一化/折叠/拒绝全取消、`worker_launch_args`
掩码→参数、proto `SetVolumes` roundtrip、worker `apply_volume_mask` 纯函数、
引擎「未选中快照不加载」与「状态按盘符排序」。
