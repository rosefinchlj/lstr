#Requires -Version 7.0
#Requires -RunAsAdministrator
<#
.SYNOPSIS
    宿主机侧 minidump 监控（tools/soak 三件套之三）：盯 VM 内的蓝屏产物。

.DESCRIPTION
    通过 PowerShell Direct（Enter-PSSession -VMName 同机制，无需网络）轮询
    VM 内 C:\Windows\Minidump\：
      - 发现新 .dmp → 自动复制到宿主 -OutDir，并计算 SHA256 供证据链使用；
      - 读取 System 日志 BugCheck 事件（ID 1001），解析 bugcheck 代码与参数；
      - 每个发现/事件追加一行 JSON 到 -OutDir\minidump-watch.jsonl（JSONL 格式）。

    VM 蓝屏重启会断开 PowerShell Direct 会话——脚本按指数退避自动重连，
    重连成功后继续监控（重启后 Minidump 才落盘，因此必须坚持轮询）。

.EXAMPLE
    .\watch-minidump.ps1 -VmName 'lstr-soak-01' -Credential (Get-Credential) -OutDir 'D:\soak-out'

.EXAMPLE
    .\watch-minidump.ps1 -VmName 'lstr-soak-01' -Credential $cred -OutDir 'D:\soak-out' -DurationSec 7200

.NOTES
    运行前提与风险：需要管理员权限、Hyper-V 模块、VM 已开机且凭据是 VM 内
    管理员账户（PowerShell Direct 要求）；-DurationSec 0（默认）表示持续监控
    直到 Ctrl+C；脚本只读 VM 内 dump 与事件日志，并向宿主 -OutDir 写副本。
#>

[CmdletBinding()]
param(
    # 目标 VM 名（即 New-SoakVM.ps1 创建的 soak VM）
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$VmName,

    # VM 内管理员账户凭据（PowerShell Direct 用）
    [Parameter(Mandatory = $true)]
    [pscredential]$Credential,

    # 宿主输出目录（dump 副本 + JSONL 日志；不存在会自动创建）
    [Parameter(Mandatory = $true)]
    [string]$OutDir,

    # 轮询间隔（秒）
    [Parameter()]
    [ValidateRange(5, 3600)]
    [int]$PollIntervalSec = 15,

    # 监控总时长（秒）；0 = 无限，直到 Ctrl+C
    [Parameter()]
    [ValidateRange(0, 2592000)]
    [int]$DurationSec = 0
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# VM 内 Minidump 目录与 BugCheck 事件提供程序
$script:DumpDirInVm = 'C:\Windows\Minidump'
$script:BugcheckProviders = @('Microsoft-Windows-WER-SystemErrorReporting', 'BugCheck')

# 向 JSONL 追加一行（ConvertTo-Json 压缩为单行）
function Add-JsonLine {
    param([hashtable]$Record, [string]$JsonlPath)
    $Record.ts = (Get-Date).ToString('o')
    $line = $Record | ConvertTo-Json -Compress -Depth 4
    Add-Content -LiteralPath $JsonlPath -Value $line -Encoding utf8
    Write-Host $line
}

try {
    # --- 前置校验 -------------------------------------------------------------
    if (-not (Get-Command Get-VM -ErrorAction SilentlyContinue)) {
        throw '未检测到 Hyper-V PowerShell 模块，请在宿主机运行本脚本。'
    }
    $vm = Get-VM -Name $VmName -ErrorAction SilentlyContinue
    if (-not $vm) { throw "找不到 VM「$VmName」。请先用 New-SoakVM.ps1 创建并启动。" }

    [void][System.IO.Directory]::CreateDirectory($OutDir)
    $jsonlPath = Join-Path $OutDir 'minidump-watch.jsonl'

    # --- 建立初始 PowerShell Direct 会话 --------------------------------------
    function New-DirectSession {
        try {
            $s = New-PSSession -VMName $VmName -Credential $Credential -ErrorAction Stop
            return $s
        }
        catch {
            return $null   # VM 关机/重启中/凭据错——交给主循环重试
        }
    }

    Write-Host "[watch] 连接 VM「$VmName」(PowerShell Direct) ..."
    $session = New-DirectSession
    if ($session) { Write-Host '[watch] 已连接。' }
    else { Write-Warning '[watch] 首次连接失败（VM 未就绪？），将继续重试。' }

    # --- 基线：已存在的 minidump 不重复抓取 ------------------------------------
    $known = @{}   # key = "Name|Length"
    $seenEventIds = @{}   # BugCheck 事件 RecordId 去重
    $lastEventCheck = (Get-Date).ToUniversalTime().AddMinutes(-5)

    Write-Host ("[watch] 开始监控: dump 目录 {0} / 轮询 {1}s / 时长 {2}s (0=无限)" -f `
                $script:DumpDirInVm, $PollIntervalSec, $DurationSec)

    $deadline = if ($DurationSec -gt 0) { [DateTime]::UtcNow.AddSeconds($DurationSec) } else { [DateTime]::MaxValue }
    $backoffSec = 0

    while ([DateTime]::UtcNow -lt $deadline) {
        try {
            if (-not $session) {
                Start-Sleep -Seconds ([Math]::Min($backoffSec, 120))
                $backoffSec = if ($backoffSec -eq 0) { $PollIntervalSec } else { $backoffSec * 2 }
                $session = New-DirectSession
                if ($session) {
                    Write-Host '[watch] 已重新连接 VM。'
                    $backoffSec = 0
                }
                continue
            }

            # ---- 1) VM 内 minidump 目录 ----
            $dumps = Invoke-Command -Session $session -ScriptBlock {
                param($dumpDir)
                Get-ChildItem -LiteralPath $dumpDir -Filter '*.dmp' -File -ErrorAction SilentlyContinue |
                    Select-Object Name, Length, LastWriteTimeUtc
            } -ArgumentList $script:DumpDirInVm

            foreach ($d in $dumps) {
                $key = '{0}|{1}' -f $d.Name, $d.Length
                if ($known.ContainsKey($key)) { continue }
                $known[$key] = $true

                $localPath = Join-Path $OutDir $d.Name
                Copy-Item -FromSession $session -Path (Join-Path $script:DumpDirInVm $d.Name) `
                          -Destination $localPath -ErrorAction Stop
                $sha = (Get-FileHash -LiteralPath $localPath -Algorithm SHA256).Hash

                Add-JsonLine -JsonlPath $jsonlPath -Record @{
                    kind     = 'minidump_new'
                    vm       = $VmName
                    file     = $d.Name
                    size     = $d.Length
                    lastWriteUtc = $d.LastWriteTimeUtc.ToString('o')
                    local    = $localPath
                    sha256   = $sha
                }
                Write-Warning ("[watch] 捕获新 minidump: {0} ({1:n0} bytes) -> {2}" -f $d.Name, $d.Length, $localPath)
            }

            # ---- 2) BugCheck 1001 事件（含 dump 落盘前的记录）----
            $events = Invoke-Command -Session $session -ScriptBlock {
                param($providers, $since)
                foreach ($prov in $providers) {
                    Get-WinEvent -FilterHashtable @{
                        LogName      = 'System'
                        ProviderName = $prov
                        Id           = 1001
                        StartTime    = $since
                    } -ErrorAction SilentlyContinue
                }
            } -ArgumentList $script:BugcheckProviders, $lastEventCheck

            foreach ($e in $events) {
                # 两个 provider 都查过时可能重复：按事件 RecordId 去重
                if ($seenEventIds.ContainsKey($e.RecordId)) { continue }
                $seenEventIds[$e.RecordId] = $true

                # 消息形如 "…检测错误为: 0x0000003b (0xc0000005, 0x…, 0x…, 0x…) …"
                $code = $null; $params = $null
                if ($e.Message -match '0x[0-9A-Fa-f]+\s*\(([^)]*)\)') {
                    $raw = $Matches[1]
                    $params = ($raw -split ',') | ForEach-Object { $_.Trim() }
                    if ($e.Message -match '检测错误为:\s*(0x[0-9A-Fa-f]+)') { $code = $Matches[1] }
                    elseif ($e.Message -match 'A reboot\s*(0x[0-9A-Fa-f]+)') { $code = $Matches[1] }
                    else { $code = $params[0] }
                }
                Add-JsonLine -JsonlPath $jsonlPath -Record @{
                    kind        = 'bugcheck_1001'
                    vm          = $VmName
                    timeCreated = $e.TimeCreated.ToString('o')
                    provider    = $e.ProviderName
                    bugcheckCode    = $code
                    bugcheckParams  = $params
                    rawMessage  = $e.Message
                }
            }
            if ($events) { $lastEventCheck = ($events | Measure-Object TimeCreated -Maximum).Maximum.ToUniversalTime() }

            # ---- 3) 正常轮询节拍 ----
            $backoffSec = 0
            Start-Sleep -Seconds $PollIntervalSec
        }
        catch {
            # 会话断开（VM 蓝屏重启中）是常态：记一条 JSONL，走重连退避
            Write-Warning ("[watch] 轮询失败（VM 可能正在蓝屏重启）: {0}" -f $_.Exception.Message)
            Add-JsonLine -JsonlPath $jsonlPath -Record @{
                kind   = 'session_error'
                vm     = $VmName
                error  = $_.Exception.Message
            }
            if ($session) { $session | Remove-PSSession -ErrorAction SilentlyContinue }
            $session = $null
            $backoffSec = if ($backoffSec -eq 0) { $PollIntervalSec } else { $backoffSec * 2 }
        }
    }

    if ($session) { $session | Remove-PSSession -ErrorAction SilentlyContinue }
    Write-Host ("[watch] 结束。JSONL: $jsonlPath")
    Write-Host '运行前提与风险：需宿主管理员权限 + Hyper-V 模块 + VM 内管理员凭据（PowerShell Direct）；脚本只读 VM 内 dump/事件日志并向宿主 -OutDir 写副本，无其他系统改动。'
}
catch {
    Write-Error "watch-minidump 失败: $($_.Exception.Message)"
    exit 1
}
