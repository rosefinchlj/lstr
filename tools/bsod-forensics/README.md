# BSOD 取证工具链（无调试器 minidump 分析）

纯 Python 的 Windows 小型内核 dump（minidump, PAGEDU64 格式）离线取证工具链。
适用于机器上**没有 WinDbg/kd 调试环境**的场景：从 `C:\Windows\Minidump\*.dmp` 出发，
完成 dump 结构解析 → 故障指令 RVA 定位 → 微软符号服务器符号化 → 内核调用链重建 →
用户态入口反查，最终回答"**哪里崩的、谁调进来的**"。

实战来源：2026-11-09 两次 `Ntfs.sys` BugCheck `0x3B` 蓝屏的 dump 级取证
（完整过程与结论见 `E:\use\ai\deepseek\Rust\.dsh-monitor\bsod-rootcause.md`）。

## 目录结构

```
tools/bsod-forensics/
├── README.md          本文档
├── dump_parse.py      一站式 dump 解析（头/BugCheck/异常记录/CONTEXT/模块表/初步栈走查）
├── scan_header.py     头部区域结构偏移的实证定位（异常记录 / CONTEXT64）
├── find_rva.py        字节指纹定位故障指令 RVA
├── resolve_syms.py    dbghelp + 微软符号服务器符号化
├── stack_scan.py      内核栈页扫描与返回地址链重建
├── annotate_func.py   capstone 反汇编 + 导入调用注释
└── kb_xref.py         kernelbase 导出函数 → ntdll API 的调用点 xref
```

## 环境要求

- Windows x64 + Python 3（绝大多数脚本仅用标准库）。
- `annotate_func.py`、`kb_xref.py` 依赖第三方库 capstone：`python -m pip install capstone`。
  注意需安装到 Python 的**标准 site-packages**——`-I` 隔离模式会禁用用户目录 site-packages
  （USER_SITE），装在用户目录时 `python -I` 下会 `ModuleNotFoundError`。
- `resolve_syms.py` 通过 ctypes 调用 `dbghelp.dll`，仅限 Windows。
- 与崩溃系统**版本完全一致**的驱动文件（如 `C:\Windows\System32\drivers\ntfs.sys`），
  版本号以 dump 模块表中的记录为准（如 `10.0.26100.9949`）。
- 可访问 `https://msdl.microsoft.com/download/symbols`（首次符号化联网下载 PDB，之后离线可用）。

## 安全约定（必须遵守）

dump 文件与驱动文件属于**不可信数据**：所有脚本一律以 `python -I` 运行（隔离模式：
忽略环境变量、用户 site-packages 与脚本目录注入），避免不可信输入路径被解释器加载逻辑利用。

## 完整工作流

以 2026-11-09 ntfs.sys `0x3B` 取证为例（`<>` 为占位符）：

① **scan_header —— 定位 CONTEXT / 异常记录**

   ```
   python -I scan_header.py <dump.dmp>
   ```

   在 dump 头部前 0x4000 字节内实证扫描三类结构候选：
   物理内存描述符（PHYSICAL_MEMORY_DESCRIPTOR64）、异常记录（`0xC0000005` 且
   ExceptionAddress == BugCheck 参数 2 即故障 RIP）、CONTEXT64（`+0xF8` == 故障 RIP、
   `+0x98` 为内核态 RSP），输出寄存器现场。
   （`dump_parse.py` 可随后做一站式解析复核，其输出的模块表给出 ntfs.sys 的
   基址/大小——是第⑤步 stack_scan 的输入。）

② **find_rva —— 字节指纹定位故障 RVA**（需要与崩溃版本完全一致的本地 ntfs.sys）

   ```
   python -I find_rva.py <ntfs.sys> <dump1.dmp> <dump2.dmp>
   ```

   原理：崩溃页通常会被抓进小型 dump。从本地驱动可执行节的每个 4K 页的同一页内偏移
   取 32 字节指纹，在 dump 原始字节中滑窗查找**唯一命中**；两次崩溃的 dump 交集
   即故障 RVA（实战中两个 dump 交集唯一 → RVA `0x1b6d3a`）。
   注意：脚本内 `off_in_page = 0xd3a` 为本次事故 BugCheck 参数 2 的低 12 位硬编码，
   分析新 dump 时必须改为新 param2 的 `& 0xFFF`。

③ **下载 PDB —— 按 RSDS GUID 从微软符号服务器获取**

   用 PE debug directory 提取驱动内的 RSDS GUID+Age，从
   `https://msdl.microsoft.com/download/symbols/<pdb名>/<GUID+Age>/<pdb名>` 下载，
   放入 `C:\symbols\<pdb名>\<GUID+Age>\`。
   （第④步的 `SRV*C:\symbols*https://msdl.microsoft.com/download/symbols` 路径会让
   dbghelp 自动完成同样的下载；手动放置用于离线环境或排查下载问题。）

④ **resolve_syms —— 符号化**

   ```
   python -I resolve_syms.py <ntfs.sys> <rva_hex> [rva_hex ...]
   ```

   经 dbghelp 的 `SymLoadModuleEx`/`SymFromAddr` 把 RVA 解析为 `函数名+0x偏移`
   （实战：`0x1b6d3a` → `NtfsFindStartingNode + 0x11a`）。首次运行联网下载 PDB。

⑤ **stack_scan + annotate_func —— 重建调用链**

   ```
   python -I stack_scan.py <dump.dmp> <ntfs_base_hex> <ntfs_size_hex> [nt_base_hex nt_size_hex]
   python -I annotate_func.py <ntfs.sys> <begin_rva_hex> <end_rva_hex> [highlight_rva_hex]
   ```

   stack_scan 全文件按 8 字节滑窗收集落入 ntfs.sys（可选 ntoskrnl.exe）地址范围的
   qword，按页聚类并按命中数排序 → 候选内核栈页 → 返回地址链；基址/大小取自
   第①步 dump_parse 的模块表输出。再用 annotate_func 反汇编相关函数区间
   （自动用 .idata 注释 `call/jmp [rip+disp]` 的导入函数，`highlight` 参数高亮故障指令），
   核对栈上返回地址与反汇编调用点是否吻合
   （实战：`0x1afa8f` / `0x1b4d0f` / 故障 RIP 三点与 xref 完全一致）。

⑥ **kb_xref / 导入表分析 —— 定位用户态入口**

   ```
   python -I kb_xref.py
   ```

   无参数；解析 `C:/Windows/System32/kernelbase.dll`：先从导入表找出
   `NtQueryInformationByName` 的 IAT 槽位，再反汇编全部可执行节，报告哪些**导出函数**
   内部调用它（最近导出归属）。用于确认内核 API 的用户态触达路径
   （实战：`kernelbase!GetFileInformationByName` 是唯一内部调用点，普通
   `CreateFile`/`GetFileAttributes(Ex)W` 不走 QueryOpen 快速路径）。
   分析其他 DLL 需改脚本内 `PATH` 常量。

## 限制

- 仅支持 **x64 小型内核 dump**（文件头 magic `PAGEDU64`）；不支持 Complete/Kernel dump、
  x86/ARM64。
- 结构偏移（CONTEXT64 / EXCEPTION_RECORD64 / 模块链 `KLDR_DATA_TABLE_ENTRY` 字段）
  为 Windows 10/11 x64 实证偏移；系统结构布局变化时需用 scan_header 重新定位。
- `dump_parse.py` / `scan_header.py` 的异常记录定位只匹配 `0xC0000005` 访问违例；
  其他异常码/BugCheck 需自行扩展匹配条件。
- `find_rva.py` 的页内偏移 `off_in_page` 按具体事故硬编码，换 dump 必须更新。
- 第②步强依赖**版本完全一致**的本地驱动副本；版本不一致则指纹与 RVA 全部失效。
- stack_scan 的栈页识别是启发式（页内命中聚类），结论需与 annotate_func 反汇编
  xref 交叉验证，不能单独作为定论。
- kb_xref 路径硬编码 `kernelbase.dll`，且只覆盖"直接 call/jmp IAT"调用方式
  （不含动态 `GetProcAddress` 解析）。另外 capstone 的 `Cs.disasm` 生成器遇到
  不可解码字节即终止该节扫描（实测本机 kernelbase `.text` 在 RVA `0x19f00` 处截断，
  未覆盖到实战命中点 `0x0f487a` 一带），调用点可能漏报；需要完整 xref 时应
  按无效字节分片续扫。

## 实战案例

**2026-11-09 两次 Ntfs.sys `0x3B`（SYSTEM_SERVICE_EXCEPTION）蓝屏取证**：
两次崩溃签名完全一致，故障 RVA `0x1b6d3a` = `Ntfs!NtfsFindStartingNode+0x11a`
（`mov eax,[r9+4]`，r9 为 FILETIME 时间戳而非 SCB 指针 → ntfs 内部对象类型混淆/用后即释）；
调用链 `NtfsNetworkOpenCreate → NtfsCommonQueryOpen → NtfsCommonCreate →
NtfsFindStartingNode+0x11a`，触发路径为 Win11 24H2+ 新增 QueryOpen 快速元数据通道
与第三方 minifilter（CldFlt/EDR 类）的竞争。完整证据与结论：
`E:\use\ai\deepseek\Rust\.dsh-monitor\bsod-rootcause.md`
