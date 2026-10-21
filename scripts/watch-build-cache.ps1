#requires -Version 5.1
<#
.SYNOPSIS
  构建写入触发：target 有写入且构建结束后，自动执行一次缓存保留清理。

.DESCRIPTION
  不使用定时轮询。FileSystemWatcher 通过 ReadDirectoryChangesW 接收系统级
  文件变更通知，平时不扫描磁盘；只有 target 下发生写入后，才在 cargo/rustc
  退出且写入静默 N 秒后调用 prune-build-cache.ps1 -Keep 3 -Apply。
  只处理 target\debug 的写入；忽略 build-prune.log 自身，避免日志触发清理。
  单实例运行；由计划任务 YunYi-build-cache-watcher 登录时启动。

  注意：cargo 自身在构建期间会持有 target 锁；这里用 cargo/rustc 进程判断，
  不把常驻的 rust-analyzer 当作“构建中”，否则编辑器一开清理就永远不执行。
#>
[CmdletBinding()]
param(
    [int]$DebounceSeconds = 30,
    [int]$Keep = 3
)

$ErrorActionPreference = 'Stop'
$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$repo = Split-Path -Parent $scriptDir
$targetRoot = Join-Path $repo 'src-tauri\target'
$prune = Join-Path $scriptDir 'prune-build-cache.ps1'
$log = Join-Path $targetRoot 'build-prune.log'

function Write-WatchLog([string]$line) {
    $stamp = Get-Date -Format 'yyyy-MM-dd HH:mm:ss'
    $text = "[$stamp] [watch] $line"
    Write-Output $text
    try {
        if (Test-Path $targetRoot) {
            Add-Content -LiteralPath $log -Value $text -Encoding UTF8 -ErrorAction Stop
        }
    } catch { }
}

if (-not (Test-Path $prune)) {
    Write-WatchLog "找不到清理脚本: $prune"
    exit 2
}

# 单实例：同一登录会话只保留一个监听器。
$mutex = New-Object System.Threading.Mutex($false, 'Local\YunYi-build-cache-watcher')
$owned = $false
try {
    $owned = $mutex.WaitOne(0, $false)
} catch [System.Threading.AbandonedMutexException] {
    $owned = $true
}
if (-not $owned) {
    Write-WatchLog '已有监听器在运行，退出'
    exit 0
}

function Test-BuildBusy {
    # rust-analyzer 空闲时也常驻，不能作为构建信号；它真正编译时会拉起 cargo/rustc。
    # cargo run 在应用运行期间常驻，但此时构建已经结束，不算“构建中”。
    if (Get-Process -Name rustc -ErrorAction SilentlyContinue) { return $true }
    try {
        $cargos = @(Get-CimInstance Win32_Process -Filter "Name='cargo.exe'" -ErrorAction Stop)
    } catch {
        return [bool](Get-Process -Name cargo -ErrorAction SilentlyContinue)
    }
    foreach ($c in $cargos) {
        $cmd = [string]$c.CommandLine
        if ($cmd -match '(?i)(^|\s)run(\s|$)') { continue }
        return $true
    }
    return $false
}

Write-WatchLog ("监听启动: {0}（写入静默 {1} 秒后清理，保留最新 {2} 代）" -f $targetRoot, $DebounceSeconds, $Keep)

$wasMissing = $false
while ($true) {
    if (-not (Test-Path $targetRoot)) {
        if (-not $wasMissing) {
            Write-WatchLog "target 不存在，等待它出现: $targetRoot"
            $wasMissing = $true
        }
        Start-Sleep -Seconds 30
        continue
    }
    if ($wasMissing) {
        Write-WatchLog "target 已出现，重新开始监听"
        $wasMissing = $false
    }

    $watcher = $null
    try {
        $watcher = New-Object System.IO.FileSystemWatcher
        $watcher.Path = $targetRoot
        $watcher.IncludeSubdirectories = $true
        $watcher.NotifyFilter = [System.IO.NotifyFilters]::FileName -bor [System.IO.NotifyFilters]::DirectoryName -bor [System.IO.NotifyFilters]::LastWrite -bor [System.IO.NotifyFilters]::Size
        $watcher.InternalBufferSize = 65536

        $pending = $false
        $lastWrite = Get-Date
        $busyLogged = $false

        while ($true) {
            try {
                $r = $watcher.WaitForChanged([System.IO.WatcherChangeTypes]::All, 2000)
            } catch [System.IO.InternalBufferOverflowException] {
                # 大构建事件太多，缓冲区溢出；仍视为有写入，静默后再清理。
                $pending = $true
                $lastWrite = Get-Date
                continue
            }

            if (-not $r.TimedOut) {
                $name = [string]$r.Name
                # 忽略清理脚本自己的日志，避免“清理→写日志→再清理”的自激。
                if ($name -like 'build-prune.log*') { continue }
                # 只关心 debug 构建（清理脚本也只清理 debug）；release 等写入不触发。
                if ($name -ne 'debug' -and $name -notlike 'debug\*') { continue }
                $pending = $true
                $lastWrite = Get-Date
                $busyLogged = $false
                continue
            }

            if (-not $pending) { continue }
            if (((Get-Date) - $lastWrite).TotalSeconds -lt $DebounceSeconds) { continue }

            if (Test-BuildBusy) {
                if (-not $busyLogged) {
                    Write-WatchLog '写入已静默，但构建进程仍在运行；等它结束后再清理'
                    $busyLogged = $true
                }
                Start-Sleep -Seconds 10
                continue
            }

            $pending = $false
            $busyLogged = $false
            Write-WatchLog ("写入已静默 {0} 秒，执行缓存清理" -f $DebounceSeconds)
            try {
                & $prune -Keep $Keep -Apply
            } catch {
                Write-WatchLog ("清理脚本异常: {0}" -f $_.Exception.Message)
            }
            # 清理产生的删除事件会再次进入待处理状态；再等一轮静默后跑一次空清理，
            # 不会形成循环（清理脚本只写 target 根目录日志，已被忽略）。
            $lastWrite = Get-Date
        }
    } catch {
        Write-WatchLog ("监听异常，10 秒后重启: {0}" -f $_.Exception.Message)
        Start-Sleep -Seconds 10
    } finally {
        if ($watcher) { $watcher.Dispose() }
    }
}
