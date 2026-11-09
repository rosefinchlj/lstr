# Feedback Hub 报告底稿：Win11 26H2 ntfs.sys 0x3B（QueryOpen 快速路径类型混淆）

> 用途：向微软反馈中心（Feedback Hub）提交蓝屏报告的中文底稿。
> 正文各节可直接复制粘贴；附件两个 minidump 需在提交界面手动附加（见「附件清单」）。
> 依据：本机两个 minidump 的取证分析（无 WinDbg，自研解析工具链，方法见文末），日期 2026-11-09。

---

## 一、标题建议（任选其一，一句话）

1. **Win11 26H2 ntfs.sys 0x3B SYSTEM_SERVICE_EXCEPTION：QueryOpen 快速路径（NtfsFindStartingNode）把 FILETIME 当指针解引用，疑似对象类型混淆/UAF**
2. Windows 11 26H2 ntfs.sys（26100.9949）在 IRP_MJ_NETWORK_QUERY_OPEN 路径崩溃（0x3B / 0xC0000005），两次崩溃同指令同 RVA

建议用第 1 条（信息量更足）。

---

## 二、正文（可整段复制进反馈中心描述框）

### 1. 系统信息

- Windows 11 26H2，build **26300**，UBR **9818**（ge_release 正式版）。
- 出问题的内核模块：**ntfs.sys 10.0.26100.9949**（文件时间 2026-11-05 12:39，随 2026-11-05 的 KB5132402 落地）。
- 两次崩溃均发生在该更新后第 4 天（2026-11-09），且**故障指令完全相同**（见下）。

### 2. 两次崩溃记录

| # | 时间（System 日志重启记录） | BugCheck | 失败模块 | Minidump |
|---|---|---|---|---|
| 1 | 2026-11-09 09:46:09 | 0x3B SYSTEM_SERVICE_EXCEPTION（0xC0000005, 0xfffff8031f546d3a, 〈context 记录地址〉, 0） | Ntfs.sys | 110926-12890-01.dmp |
| 2 | 2026-11-09 10:31:00 | 0x3B SYSTEM_SERVICE_EXCEPTION（0xC0000005, 0xfffff800133a6d3a, 〈context 记录地址〉, 0） | Ntfs.sys | 110926-12171-01.dmp |

- 两个 faulting address 低 16 位相同（…6d3a），ntfs.sys KASLR 基址分别为
  0xfffff8031f390000（#1）与 0xfffff800131f0000（#2），相减得**同一 RVA：0x1b6d3a**。
  即两个 dump 崩在 ntfs.sys 内同一条指令。

### 3. 关键取证结论（供 MS 工程师复核）

说明：本地无 WinDbg/cdb，以下结论由自研解析工具链得出（PAGEDU64 头解析、字节指纹定位故障 RVA、栈扫描重建调用链、dbghelp + 微软符号服务器符号化、PE 导入表交叉验证）。如需原始数据我可以提供。

1. **故障指令**：`Ntfs!NtfsFindStartingNode+0x11a`（RVA 0x1b6d3a）：`mov eax,[r9+4]`。
2. **CONTEXT64 关键寄存器**：`R9 = 0x01dd6ff23eba521a`，`R13 = 0x01dd6ff23e8eb368`。
   这两个值都在 2026 年区间的 **FILETIME 数值范围内**，却被当作指针解引用（`[r9+4]` 读访问违例）。
   值来源于 `[rcx+0xc0]` / `[rcx+0xb8]`——代码期望该处是（SCB 相关的）指针，实际读到的是时间戳字段。
   这指向 **ntfs 内部对象类型混淆（type confusion）或用后即释（UAF）**，而非用户态传入参数问题。
3. **完整符号化调用链**（QueryOpen 快速路径）：

   ```
   IRP_MJ_NETWORK_QUERY_OPEN
   → NtfsNetworkOpenCreate        (0x1af910, 包装层)
   → NtfsCommonQueryOpen          (0x1af940)
   → NtfsCommonCreate             (0x1b36a0, call @0x1b4d0a)
   → NtfsFindStartingNode         (0x1b6c20)
   → 崩溃于 NtfsFindStartingNode+0x11a (0x1b6d3a)
   ```

   栈上返回地址 0x1afa8f / 0x1b4d0f / 故障 RIP 三点一线，与反汇编交叉引用完全吻合。
4. **谁能进入这条路径**（QueryOpen 是 Win11 24H2 起新增的免打开句柄元数据快速通道）：
   - 用户态：`kernelbase!GetFileInformationByName`（经 xref 验证其内部仅通过 `NtQueryInformationByName` 下内核，调用点 @0x0f487a）。
   - 内核态：`FltQueryInformationByName`（minifilter 使用）。
   - 普通 `CreateFile` / `GetFileAttributes(Ex)W` / `GetFileInformationByHandle` **不走**此路径
     （kernelbase 内无转发，已验证）。
5. **本机过滤器环境**：注册表枚举约 20 个第三方 minifilter，包括深信服 EDR
   （sfesp / sfeknl / sfavflt / SfavBoot）、天锐 DLP/加密（TsdEncrypt / TsdEncryptMF / TFsdframe）、
   CldFlt（OneDrive 云文件过滤器）等。其中 **CldFlt.sys 静态导入 `FltQueryInformationByName`**；
   sf*/Tsd* 系列均导入 `FltQueryInformationFile` + `FltGetFileNameInformation`。
   两个崩溃 dump 的模块表中可辨认出 sfesp.sys / sfeknl.sys。
6. **我方的应用不是该内核调用的发起者**：崩溃时段运行的扫描工具（源码快照、全部依赖、
   Rust 标准库）均不包含 `GetFileInformationByName` / `NtQueryInformationByName`；
   发布二进制中相关字符串计数为 0。它只是制造了大量文件元数据活动（第三方过滤器
   对这些活动发起 QueryOpen 查询，进入该崩溃路径）。

### 4. 复现条件

- 全卷文件元数据高负载期间（目录枚举 / USN Journal 轮询类扫描工具全速运行），
  叠加第三方 minifilter 活动。
- 两次崩溃均发生在同一场景：全卷扫描运行中（时间线见附件 dump 与 System 日志）。
- 未在静态/低负载时段观察到崩溃；未确认最小复现步骤。

### 5. 附件清单（提交时手动附加）

- `C:\Windows\Minidump\110926-12890-01.dmp`（#1，2026-11-09 09:46）
- `C:\Windows\Minidump\110926-12171-01.dmp`（#2，2026-11-09 10:31，约 5452 KB）

### 6. 请求

- 请排查 ntfs.sys QueryOpen 快速路径（NtfsNetworkOpenCreate → NtfsCommonQueryOpen →
  NtfsCommonCreate → NtfsFindStartingNode）中的对象类型混淆/UAF 问题——
  现象是 `[rcx+0xb8/0xc0]` 期望指针却读到 FILETIME 值并在 `NtfsFindStartingNode+0x11a` 解引用崩溃。
- 该路径可被第三方 minifilter 经 `FltQueryInformationByName` 或应用经
  `GetFileInformationByName` 触达，崩溃不带用户可感知的前兆，影响数据安全。
- 如需更多数据（完整寄存器/栈提取、符号化细节、ntfs.sys 与过滤器的版本清单），
  我可以提供。

---

## 三、提交操作提示（不进正文）

1. Feedback Hub →「报告问题」→ 类别选 Windows / 稳定性（蓝屏）。
2. 正文粘贴上面「二」的全部内容（保留分节与小标题）。
3. 「附加文件」手动添加两个 minidump（路径见上）；如界面限制单个文件大小，优先附 #2。
4. 提交后记录反馈 ID，便于后续跟进补充数据。
