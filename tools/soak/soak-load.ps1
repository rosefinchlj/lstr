#Requires -Version 7.0
#Requires -RunAsAdministrator
<#
.SYNOPSIS
    VM 内 soak 压测负载（tools/soak 三件套之二）。设计为【只在 VM 内】运行。

.DESCRIPTION
    并行施加三类负载，用于验证 lstr USN 增量层健壮性，并尝试复现
    Win11 26H2 ntfs.sys QueryOpen 快速路径的 0x3B 蓝屏（取证结论：需
    「重元数据负载 × QueryOpen 调用」场景）：

      ① 文件搅动   -WorkDir 下多线程循环 create/rename/delete（纯 .NET API）
      ② QueryOpen  P/Invoke kernelbase!GetFileInformationByName 循环随机查询
         （这是 24H2+ 新增的免开句柄元数据快速通道，即崩溃栈所在路径；
          PowerShell 7 无此 API 封装，故用 Add-Type 声明）
      ③ USN 轮询   可选 -UsnProbeExe：循环拉起 lstr 的 probe 可执行文件

    结束时输出各负载速率统计。所有临时文件在 -WorkDir 下，脚本结束时清理。

.EXAMPLE
    pwsh -NoProfile -File soak-load.ps1 -WorkDir 'C:\soak-work' -DurationSec 1800

.EXAMPLE
    pwsh -NoProfile -File soak-load.ps1 -WorkDir 'C:\soak-work' -DurationSec 3600 -UsnProbeExe 'C:\tools\lstr-usn-probe.exe' -UsnProbeArgs '--vol C:'

.NOTES
    运行前提与风险：仅限无第三方 minifilter 的干净 VM 内、以管理员运行；
    压测目标本身是尝试触发 OS 级 bug 收集 dump——运行可能导致 VM 蓝屏/重启、
    磁盘 IO 与 CPU 占满；严禁在宿主机或生产机器上运行。
#>

[CmdletBinding()]
param(
    # 负载工作目录（搅动文件与查询池都放在这里；不存在会自动创建）
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$WorkDir,

    # 总压测时长（秒），三类负载统一到同一截止时间
    [Parameter()]
    [ValidateRange(10, 86400)]
    [int]$DurationSec = 1800,

    # 文件搅动并行线程数
    [Parameter()]
    [ValidateRange(1, 64)]
    [int]$ChurnThreads = 4,

    # QueryOpen 压测并行线程数
    [Parameter()]
    [ValidateRange(1, 64)]
    [int]$QueryOpenThreads = 2,

    # 可选：lstr 的 USN probe 可执行文件路径；提供则循环拉起做 USN 轮询
    [Parameter()]
    [string]$UsnProbeExe = '',

    # 可选：probe 的命令行参数（原样透传）
    [Parameter()]
    [string]$UsnProbeArgs = ''
)

# =============================================================================
# 安全边界（务必先读）
#   1. 本脚本【只允许在 Hyper-V VM 内运行】；压测目标就是尝试触发 OS bug（ntfs.sys
#      QueryOpen 路径 0x3B）并收集 minidump——蓝屏是「预期内的实验结果」，不是事故。
#   2. VM 必须干净：不装任何第三方杀软/DLP/EDR/minifilter，用于对照
#      「无第三方过滤器时不复现」，反证本机崩溃需要 minifilter 参与。
#   3. -WorkDir 下的文件会被高频创建/改名/删除，脚本结束时自动清理搅动目录。
#   4. 严禁在宿主机或任何生产/个人数据盘上运行。
# =============================================================================

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# --- QueryOpen P/Invoke 声明 -------------------------------------------------
# 签名来源（Microsoft Learn，winbase.h）：
#   BOOL GetFileInformationByName(
#         PCWSTR FileName, FILE_INFO_BY_NAME_CLASS FileInformationClass,
#         PVOID FileInfoBuffer, ULONG FileInfoBufferSize);
# 枚举 FILE_INFO_BY_NAME_CLASS（minwinbase.h）：
#   FileStatByNameInfo=0, FileStatLxByNameInfo=1, FileCaseSensitiveByNameInfo=2,
#   FileStatBasicByNameInfo=3, MaximumFileInfoByNameClass=4
#   —— 注意类名是 FileStatByNameInfo（结构才是 FILE_STAT_INFORMATION）。
# 结构 FILE_STAT_INFORMATION（ntifs.h）：
#   7×LARGE_INTEGER(FileId/CreationTime/LastAccessTime/LastWriteTime/ChangeTime/
#   AllocationSize/EndOfFile) + 4×ULONG(FileAttributes/ReparseTag/NumberOfLinks/
#   EffectiveAccess) = 56+16 = 72 字节。
# 导出位置：文档标注 Kernel32.dll，实际实现在 kernelbase.dll（取证确认
# kernelbase!GetFileInformationByName），故 DllImport 直接指 kernelbase。
$script:CsQueryByName = @'
using System;
using System.Runtime.InteropServices;

public static class FileByNameNative
{
    // 返回 0 = 成功；非 0 = Win32 错误码（SetLastError 捕获，避免跨线程取错）
    public static int QueryStatByName(string path, byte[] buffer)
    {
        bool ok = GetFileInformationByName(path, 0 /* FileStatByNameInfo */, buffer, (uint)buffer.Length);
        return ok ? 0 : Marshal.GetLastWin32Error();
    }

    [DllImport("kernelbase.dll", EntryPoint = "GetFileInformationByName",
               SetLastError = true, CharSet = CharSet.Unicode)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool GetFileInformationByName(
        [MarshalAs(UnmanagedType.LPWStr)] string fileName,
        uint fileInformationClass,
        [Out] byte[] fileInfoBuffer,
        uint fileInfoBufferSize);
}
'@

# --- 线程体：文件搅动 ---------------------------------------------------------
$script:SbChurn = {
    param($workDir, $seed, $deadlineUtc)
    $dir = Join-Path $workDir ("churn-t{0}" -f $seed)
    [void][System.IO.Directory]::CreateDirectory($dir)
    $rng  = [System.Random]::new()
    $ops  = 0
    $recycled = 0
    while ([DateTime]::UtcNow -lt $deadlineUtc) {
        $i  = $rng.Next()
        $f1 = Join-Path $dir ("{0}.tmp" -f $i)
        $f2 = Join-Path $dir ("{0}.renamed" -f $i)
        [System.IO.File]::WriteAllText($f1, $i.ToString())
        [System.IO.File]::Move($f1, $f2)
        [System.IO.File]::Delete($f2)
        $ops += 3
        # 每 3000 个操作重建线程目录：同时搅动目录项元数据
        if (($ops % 3000) -eq 0) {
            [System.IO.Directory]::Delete($dir, $true)
            [void][System.IO.Directory]::CreateDirectory($dir)
            $recycled++
        }
    }
    [pscustomobject]@{ Role = 'churn'; Thread = $seed; Ops = $ops; DirRecycles = $recycled }
}

# --- 线程体：QueryOpen 压测 ---------------------------------------------------
# 查询目标混合三类，覆盖「存在/竞态中/不存在」：
#   openpool 稳定文件（约 1/3）、churn 线程目录下的随机名（约 1/3，常处于
#   创建/删除竞态窗口）、miss\ghost-* 不存在路径（约 1/3，走打开失败路径）。
$script:SbQueryOpen = {
    param($workDir, $seed, $deadlineUtc, $csSource, $churnThreads)
    Add-Type -TypeDefinition $csSource   # 本线程 runspace 内编译一次
    $buf = [byte[]]::new(72)             # FILE_STAT_INFORMATION 72 字节
    $rng = [System.Random]::new()
    $ok = 0; $err = 0
    $errCodes = @{}
    while ([DateTime]::UtcNow -lt $deadlineUtc) {
        switch ($rng.Next(3)) {
            0 { $path = Join-Path $workDir ("openpool\f{0}.txt" -f $rng.Next(200)) }
            1 { $path = Join-Path $workDir ("churn-t{0}\{1}.tmp" -f ($rng.Next($churnThreads) + 1), $rng.Next()) }
            default { $path = Join-Path $workDir ("miss\ghost-{0}" -f $rng.Next()) }
        }
        $rc = [FileByNameNative]::QueryStatByName($path, $buf)
        if ($rc -eq 0) { $ok++ }
        else {
            $err++
            if ($errCodes.ContainsKey($rc)) { $errCodes[$rc]++ } else { $errCodes[$rc] = 1 }
        }
    }
    $errFmt = ($errCodes.GetEnumerator() | Sort-Object Name |
               ForEach-Object { 'win32={0}:{1}' -f $_.Key, $_.Value }) -join ', '
    [pscustomobject]@{ Role = 'queryopen'; Thread = $seed; Ok = $ok; Err = $err; ErrDetail = $errFmt }
}

# --- 线程体：USN probe 循环拉起 ----------------------------------------------
$script:SbProbe = {
    param($exePath, $exeArgs, $workDir, $deadlineUtc)
    $runs = 0; $fails = 0
    while ([DateTime]::UtcNow -lt $deadlineUtc) {
        try {
            $psi = [System.Diagnostics.ProcessStartInfo]::new()
            $psi.FileName               = $exePath
            $psi.Arguments              = $exeArgs
            $psi.WorkingDirectory       = $workDir
            $psi.UseShellExecute        = $false
            $psi.RedirectStandardOutput = $true
            $psi.RedirectStandardError  = $true
            $p = [System.Diagnostics.Process]::Start($psi)
            $p.WaitForExit()
            $runs++
            if ($p.ExitCode -ne 0) { $fails++ }
            $p.Dispose()
        }
        catch { $fails++ }
    }
    [pscustomobject]@{ Role = 'usn-probe'; Runs = $runs; Fails = $fails }
}

try {
    # --- 前置准备 -------------------------------------------------------------
    [void][System.IO.Directory]::CreateDirectory($WorkDir)
    [void][System.IO.Directory]::CreateDirectory((Join-Path $WorkDir 'openpool'))
    [void][System.IO.Directory]::CreateDirectory((Join-Path $WorkDir 'miss'))

    # 建立 QueryOpen 查询池：200 个稳定存在的文件（不做搅动，专供查询）
    Write-Host "[soak] 建查询池 openpool: 200 个文件 ..."
    for ($i = 0; $i -lt 200; $i++) {
        [System.IO.File]::WriteAllText((Join-Path $WorkDir ("openpool\f{0}.txt" -f $i)), $i.ToString())
    }

    $deadline = [DateTime]::UtcNow.AddSeconds($DurationSec)
    Write-Host ("[soak] 开始: {0} (时长 {1}s, 截止 UTC {2:u})" -f `
                (Get-Date -Format 'yyyy-MM-dd HH:mm:ss'), $DurationSec, $deadline)

    # --- 启动三类并行负载（-AsJob：每类一个 job，内部分线程并行）---------------
    $churnJobs = 1..$ChurnThreads | ForEach-Object -Parallel $script:SbChurn -AsJob -Parameter @{
        workDir     = $WorkDir
        seed        = $_
        deadlineUtc = $deadline
    }
    $qoJobs = 1..$QueryOpenThreads | ForEach-Object -Parallel $script:SbQueryOpen -AsJob -Parameter @{
        workDir      = $WorkDir
        seed         = $_
        deadlineUtc  = $deadline
        csSource     = $script:CsQueryByName
        churnThreads = $ChurnThreads
    }
    $probeJobs = @()
    if ($UsnProbeExe) {
        if (-not (Test-Path -LiteralPath $UsnProbeExe -PathType Leaf)) {
            throw "probe 可执行文件不存在: $UsnProbeExe"
        }
        $probeJobs += 1 | ForEach-Object -Parallel $script:SbProbe -AsJob -Parameter @{
            exePath     = (Convert-Path -LiteralPath $UsnProbeExe)
            exeArgs     = $UsnProbeArgs
            workDir     = $WorkDir
            deadlineUtc = $deadline
        }
        Write-Host "[soak] USN probe 已启用: $UsnProbeExe"
    }

    # --- 进度打印：所有 job 自行到截止时间收工 --------------------------------
    $allJobs = @($churnJobs) + @($qoJobs) + @($probeJobs)
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $progressStep = [Math]::Min(15, [Math]::Max(5, [int]($DurationSec / 10)))
    while (($sw.Elapsed.TotalSeconds -lt $DurationSec)) {
        Wait-Job -Job $allJobs -Any -Timeout $progressStep -ErrorAction SilentlyContinue | Out-Null
        Write-Host ("[soak] 进度 {0:n0}s / {1:n0}s" -f `
                    [Math]::Min($sw.Elapsed.TotalSeconds, $DurationSec), $DurationSec)
        if (-not ($allJobs | Where-Object { $_.State -eq 'Running' })) { break }
    }

    # --- 收尾：等剩余 job，超时强停 -------------------------------------------
    Wait-Job -Job $allJobs -Timeout 15 -ErrorAction SilentlyContinue | Out-Null
    $running = @($allJobs | Where-Object { $_.State -eq 'Running' })
    if ($running.Count -gt 0) {
        Write-Warning "强制停止 $($running.Count) 个未结束的负载 job"
        $running | Stop-Job -ErrorAction SilentlyContinue
    }

    # --- 汇总统计 -------------------------------------------------------------
    $results = @()
    foreach ($j in $allJobs) {
        $results += @(Receive-Job -Job $j -ErrorAction SilentlyContinue)
        Remove-Job -Job $j -Force -ErrorAction SilentlyContinue
    }

    $elapsedSec = [Math]::Max($sw.Elapsed.TotalSeconds, 1)
    $churnOps = ($results | Where-Object Role -eq 'churn'    | Measure-Object Ops -Sum).Sum
    $qoOk     = ($results | Where-Object Role -eq 'queryopen'| Measure-Object Ok  -Sum).Sum
    $qoErr    = ($results | Where-Object Role -eq 'queryopen'| Measure-Object Err -Sum).Sum
    $probeRuns = ($results | Where-Object Role -eq 'usn-probe' | Measure-Object Runs  -Sum).Sum
    $probeFail = ($results | Where-Object Role -eq 'usn-probe' | Measure-Object Fails -Sum).Sum

    Write-Host ''
    Write-Host '==== soak 统计 ===='
    Write-Host ("实际运行: {0:n1}s" -f $elapsedSec)
    Write-Host ("文件搅动: {0:n0} 次 create/rename/delete (平均 {1:n0} ops/s)" -f $churnOps, ($churnOps / $elapsedSec))
    Write-Host ("QueryOpen: 成功 {0:n0}, 失败 {1:n0} (合计 {2:n0} 次, {3:n0} qps)" -f $qoOk, $qoErr, ($qoOk + $qoErr), (($qoOk + $qoErr) / $elapsedSec))
    foreach ($r in ($results | Where-Object Role -eq 'queryopen')) {
        if ($r.Err -gt 0) { Write-Host ("  线程 {0} 错误分布: {1}" -f $r.Thread, $r.ErrDetail) }
    }
    if ($UsnProbeExe) {
        Write-Host ("USN probe: 拉起 {0:n0} 次, 非 0 退出 {1:n0} (约 {2:n2} 次/s)" -f $probeRuns, $probeFail, ($probeRuns / $elapsedSec))
    }
    Write-Host '==================='

    # --- 清理搅动目录与查询池 -------------------------------------------------
    Write-Host '[soak] 清理 -WorkDir 下的 churn-* / openpool / miss ...'
    foreach ($pat in 'churn-*', 'openpool', 'miss') {
        Remove-Item -LiteralPath (Join-Path $WorkDir $pat) -Recurse -Force -ErrorAction SilentlyContinue
    }

    Write-Host '[soak] 完成。'
    Write-Host '运行前提与风险：本脚本仅限干净 VM 内管理员运行；蓝屏/重启是其预期内可能的实验结果，严禁在宿主机或生产机器上运行。'
}
catch {
    Write-Error "soak-load 失败: $($_.Exception.Message)"
    exit 1
}
