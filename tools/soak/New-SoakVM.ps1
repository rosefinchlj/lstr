#Requires -Version 7.0
#Requires -RunAsAdministrator
<#
.SYNOPSIS
    创建用于 soak 压测的 Hyper-V Gen2 VM（tools/soak 三件套之一）。

.DESCRIPTION
    只负责：建 VM（Gen2）→ 建数据盘（全新 VHDX 或基于父盘的差分盘）→ 挂 ISO →
    设置首选启动设备 → 打印后续手工步骤。安装操作系统必须用户交互完成，
    本脚本不做任何无人值守安装。

    两种建盘模式（二选一）：
      1) 全新安装：  -IsoPath 指向 Windows ISO，脚本创建动态 VHDX 并挂载 ISO；
      2) 差分盘复用：-ParentVhdxPath 指向已装好系统的父 VHDX，脚本只创建差分盘
         （不挂 ISO；省去重复安装系统）。

    创建完成后请按脚本末尾打印的「后续手工步骤」操作：
    装 / 检查系统 → 把 soak-load.ps1 拷进 VM → 在 VM 内以管理员运行；
    回到宿主机运行 watch-minidump.ps1 监控 minidump。

.EXAMPLE
    .\New-SoakVM.ps1 -VmName 'lstr-soak-01' -IsoPath 'D:\iso\Win11_26H2_x64.iso' -MemoryGB 4 -VhdSizeGB 64

.EXAMPLE
    .\New-SoakVM.ps1 -VmName 'lstr-soak-02' -ParentVhdxPath 'D:\vhdx\win11-26h2-clean.vhdx' -MemoryGB 8

.NOTES
    运行前提与风险：需要管理员权限与已启用 Hyper-V 的 Windows 宿主机（Get-WindowsFeature/可选功能）；
    脚本只创建 VM/磁盘文件，不会改动宿主系统其他内容；ISO 差分盘模式一旦产生写盘就依赖父盘，请勿移动或删除父盘。
#>

[CmdletBinding()]
param(
    # VM 显示名（同时用作 VHDX 所在子目录名与文件名前缀）
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$VmName,

    # Windows 安装 ISO 路径（全新安装模式；与 -ParentVhdxPath 二选一）
    [Parameter()]
    [ValidateScript({ Test-Path -LiteralPath $_ -PathType Leaf }, ErrorMessage = 'ISO 文件不存在: {0}')]
    [string]$IsoPath = '',

    # 父 VHDX 路径（差分盘模式；与 -IsoPath 二选一）
    [Parameter()]
    [ValidateScript({ Test-Path -LiteralPath $_ -PathType Leaf }, ErrorMessage = '父 VHDX 不存在: {0}')]
    [string]$ParentVhdxPath = '',

    # 启动内存（GB）
    [Parameter()]
    [ValidateRange(1, 256)]
    [int]$MemoryGB = 4,

    # 数据盘大小（GB，仅全新 VHDX 模式生效；差分盘大小继承父盘）
    [Parameter()]
    [ValidateRange(16, 1024)]
    [int]$VhdSizeGB = 64,

    # 附加的 Hyper-V 虚拟交换机名（默认交换机仅提供 NAT，够 soak 用）
    [Parameter()]
    [ValidateNotNullOrEmpty()]
    [string]$SwitchName = 'Default Switch'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

try {
    # --- 参数模式校验：ISO 与父盘必须二选一 -----------------------------------
    if (-not $IsoPath -and -not $ParentVhdxPath) {
        throw '必须指定 -IsoPath（全新安装）或 -ParentVhdxPath（差分盘）其中之一。'
    }
    if ($IsoPath -and $ParentVhdxPath) {
        throw '-IsoPath 与 -ParentVhdxPath 只能二选一。'
    }

    # --- Hyper-V 模块可用性 ---------------------------------------------------
    if (-not (Get-Command New-VM -ErrorAction SilentlyContinue)) {
        throw '未检测到 Hyper-V PowerShell 模块。请先在宿主机启用 Hyper-V 管理工具与平台。'
    }

    # 同名 VM 已存在时直接报错退出，避免误改现有 VM
    if (Get-VM -Name $VmName -ErrorAction SilentlyContinue) {
        throw "已存在同名 VM「$VmName」。请换名或先删除旧 VM。"
    }

    # --- 建 VHDX --------------------------------------------------------------
    # 存放位置取宿主机默认虚拟磁盘路径，按 VM 名分子目录，避免写死盘符
    $vhdRoot = (Get-VMHost).VirtualHardDiskPath
    $vhdDir  = Join-Path $vhdRoot $VmName
    New-Item -Path $vhdDir -ItemType Directory -Force | Out-Null
    $vhdPath = Join-Path $vhdDir "$VmName.vhdx"

    if ($ParentVhdxPath) {
        # 差分盘：所有写入进新盘，父盘只读
        Write-Host "创建差分盘: $vhdPath (父盘: $ParentVhdxPath)"
        New-VHD -Path $vhdPath -ParentPath $ParentVhdxPath -Differencing | Out-Null
    }
    else {
        Write-Host "创建动态 VHDX: $vhdPath (大小 $VhdSizeGB GB)"
        New-VHD -Path $vhdPath -SizeBytes ($VhdSizeGB * 1GB) -Dynamic | Out-Null
    }

    # --- 建 VM（Gen2，UEFI）---------------------------------------------------
    Write-Host "创建 Gen2 VM「$VmName」: 内存 ${MemoryGB}GB, 交换机 $SwitchName"
    $vm = New-VM -Name $VmName `
                 -Generation 2 `
                 -MemoryStartupBytes ($MemoryGB * 1GB) `
                 -VHDPath $vhdPath `
                 -SwitchName $SwitchName

    # 启用动态内存上限一致，避免 soak 期间宿主内存被吃穿
    Set-VMMemory -VM $vm -DynamicMemoryEnabled $true `
                 -MinimumBytes ($MemoryGB * 1GB) -MaximumBytes ($MemoryGB * 1GB)

    # --- 挂 ISO 并设为首选启动设备（仅全新安装模式）---------------------------
    if ($IsoPath) {
        Write-Host "挂载 ISO: $IsoPath"
        $dvd = Add-VMDvdDrive -VMName $VmName -Path $IsoPath
        Set-VMFirmware -VM $vm -FirstBootDevice $dvd
    }
    else {
        Write-Host '差分盘模式：不挂 ISO，直接从差分盘启动。'
    }

    Write-Host ''
    Write-Host "VM「$VmName」创建完成：$(Join-Path $vhdDir '')"
    Write-Host ''
    Write-Host '=== 后续手工步骤（安装系统需要你交互完成）==='
    Write-Host '1) 启动并连接 VM（Hyper-V 管理器或: Start-VM -Name <VmName>; vmconnect localhost <VmName>）。'
    if ($IsoPath) {
        Write-Host '2) 在 VM 内完成 Windows 全新安装（干净系统，勿装任何第三方杀软/DLP/EDR/minifilter——本压测要求无第三方过滤器）。'
    }
    else {
        Write-Host '2) 启动 VM 确认父盘系统可用；确认 VM 内无任何第三方 minifilter（fltmc filters 核对）。'
    }
    Write-Host '3) 在 VM 内创建一个管理员账户（PowerShell Direct 需要其凭据）。'
    Write-Host '4) 把 tools/soak/soak-load.ps1 拷入 VM（网络共享或拖拽 ISO 外的挂载盘均可）。'
    Write-Host "5) VM 内管理员 pwsh 运行: pwsh -NoProfile -File soak-load.ps1 -WorkDir 'C:\soak-work' -DurationSec 1800"
    Write-Host '6) 宿主机另开窗口运行: pwsh -NoProfile -File tools/soak/watch-minidump.ps1 -VmName <VmName> -Credential (Get-Credential) -OutDir <宿主输出目录>'
    Write-Host '7) soak 结束后，若无 minidump 且运行稳定，即证明「无第三方 minifilter 时不复现」；若 VM 蓝屏，watch-minidump.ps1 会自动把 dump 抓回宿主机。'
}
catch {
    Write-Error "New-SoakVM 失败: $($_.Exception.Message)"
    exit 1
}
