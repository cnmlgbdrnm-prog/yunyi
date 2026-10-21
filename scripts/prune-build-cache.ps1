#requires -Version 5.1
<#
.SYNOPSIS
  只保留本项目 src-tauri/target 里最新的 N 代构建缓存。

.DESCRIPTION
  清理范围**仅限** <repo>/src-tauri/target，不触碰源码、.git、数据库、
  node_modules，也不触碰其它盘。

  保留规则（默认 Keep=3）：
  1) deps 下按哈希分组的 vpn_ctl-<hash>* / vpn_ctl.<hash>* 产物：每组保留最新 3 组；
  2) incremental 下按 crate 前缀分组的 s-* 会话目录：每个前缀保留最新 3 个；
  3) 会话被删空的 incremental/<crate>-<hash> 顶层目录一并删除；
  4) target/debug 下 vpn-ctl.exe.old* / pre-*.exe / new-*.exe 备份：保留最新 3 个。

  默认 dry-run，仅打印将删除的内容；加 -Apply 才真正删除。
  有 rustc 或非 `cargo run` 的 cargo 在跑时直接跳过，避免破坏正在进行的构建
  （rust-analyzer 与 `cargo run` 空闲时也常驻，不能作为“正在构建”的信号）。
  日志写入 target/build-prune.log；超过 1 MB 时只保留最近 500 行。
  自动执行由 watch-build-cache.ps1 在 target 写入后触发；计划任务
  YunYi-build-cache-watcher 只在登录时启动监听器，不做定时轮询。
#>
[CmdletBinding()]
param(
    [int]$Keep = 3,
    [switch]$Apply
)

$ErrorActionPreference = 'Stop'
$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$repo = Split-Path -Parent $scriptDir
$targetRoot = Join-Path $repo 'src-tauri\target'
$debugRoot = Join-Path $targetRoot 'debug'
$deps = Join-Path $debugRoot 'deps'
$incremental = Join-Path $debugRoot 'incremental'
$log = Join-Path $targetRoot 'build-prune.log'

if ($Keep -lt 1) {
    Write-Error "Keep 必须 >= 1"
    exit 2
}

function Write-Log([string]$line) {
    $stamp = Get-Date -Format 'yyyy-MM-dd HH:mm:ss'
    $text = "[$stamp] $line"
    # 不能写成功流（Write-Output）：Remove-Tree/Test-BuildBusy 既写日志又返回
    # 删除字节数/布尔值，日志字符串会混进返回值，调用方 `$freed += Remove-Tree`
    # 就变成「整数 + 字符串数组」，报 Object[] 没有 op_Addition。
    [Console]::Out.WriteLine($text)
    if (Test-Path $targetRoot) {
        Add-Content -LiteralPath $log -Value $text -Encoding UTF8 -ErrorAction SilentlyContinue
    }
}

# 日志只保留最近一段：监听器按构建写入触发，长期运行也不能让日志无限增长。
if (Test-Path $log) {
    try {
        if ((Get-Item -LiteralPath $log).Length -gt 1MB) {
            $tail = Get-Content -LiteralPath $log -Tail 500 -ErrorAction Stop
            Set-Content -LiteralPath $log -Value $tail -Encoding UTF8
        }
    } catch { }
}

# 构建进程探测：扫描期间构建可能才开始，删除前必须再查一次。
function Test-BuildBusy {
    # 不检查 rust-analyzer：它是常驻语言服务器，空闲时也在，会把清理永久挡掉。
    # rustc 在编译时存在；cargo run 在应用运行期间常驻，但构建已结束，不算构建中。
    if (Get-Process -Name rustc -ErrorAction SilentlyContinue) {
        Write-Log '检测到 rustc，中止本次清理'
        return $true
    }
    try {
        $cargos = @(Get-CimInstance Win32_Process -Filter "Name='cargo.exe'" -ErrorAction Stop)
    } catch {
        if (Get-Process -Name cargo -ErrorAction SilentlyContinue) {
            Write-Log '无法查询 cargo 命令行，按构建中处理，中止本次清理'
            return $true
        }
        return $false
    }
    foreach ($c in $cargos) {
        $cmd = [string]$c.CommandLine
        if ($cmd -match '(?i)(^|\s)run(\s|$)') { continue }
        Write-Log ("检测到构建进程 (cargo: {0})，中止本次清理" -f $cmd)
        return $true
    }
    return $false
}

function Get-TreeBytes([string]$path) {
    try {
        $sum = (Get-ChildItem -LiteralPath $path -Recurse -Force -File -ErrorAction SilentlyContinue |
            Measure-Object -Property Length -Sum).Sum
        if ($null -eq $sum) { return [int64]0 }
        return [int64]$sum
    } catch {
        return [int64]0
    }
}

function Remove-Tree([string]$path, [string]$why) {
    $full = [System.IO.Path]::GetFullPath($path)
    if (-not $full.StartsWith([System.IO.Path]::GetFullPath($targetRoot), [System.StringComparison]::OrdinalIgnoreCase)) {
        Write-Log "拒绝删除 target 之外的路径: $full"
        return [int64]0
    }
    $bytes = Get-TreeBytes $full
    if (-not $Apply) {
        Write-Log ("DRY-RUN 将删除 {0:N1} MB: {1} ({2})" -f ($bytes / 1MB), $full, $why)
        return [int64]0
    }
    try {
        Remove-Item -LiteralPath $full -Recurse -Force -Confirm:$false -ErrorAction Stop
        Write-Log ("已删除 {0:N1} MB: {1} ({2})" -f ($bytes / 1MB), $full, $why)
        return $bytes
    } catch {
        Write-Log ("删除失败: {0} -> {1}" -f $full, $_.Exception.Message)
        return [int64]0
    }
}

if (-not (Test-Path $targetRoot)) {
    Write-Log "target 不存在，跳过: $targetRoot"
    exit 0
}

# 有构建在跑就完全跳过：宁可晚一次，也不碰正在写的产物。
if (Test-BuildBusy) { exit 0 }

$dry = -not $Apply
$volumeLetter = ([System.IO.Path]::GetPathRoot($targetRoot)).TrimEnd('\').TrimEnd(':')
$freeBefore = [int64]0
try { $freeBefore = (Get-Volume -DriveLetter $volumeLetter).SizeRemaining } catch { }
Write-Log ("开始，Keep={0}，模式={1}" -f $Keep, $(if ($dry) { 'dry-run' } else { 'apply' }))
$freed = [int64]0

# 1) deps：vpn_ctl-<hash>* / vpn_ctl.<hash>* 按哈希分组，保留最新 Keep 组。
if (Test-Path $deps) {
    $hashGroups = @{}
    Get-ChildItem -LiteralPath $deps -File -Force -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -match '^vpn_ctl[-.]([0-9a-fA-F]{8,})' } |
        ForEach-Object {
            $hash = [regex]::Match($_.Name, '^vpn_ctl[-.]([0-9a-fA-F]{8,})').Groups[1].Value.ToLower()
            if (-not $hashGroups.ContainsKey($hash)) {
                $hashGroups[$hash] = @{ Files = @(); Newest = [datetime]::MinValue }
            }
            $hashGroups[$hash].Files += $_
            if ($_.LastWriteTime -gt $hashGroups[$hash].Newest) {
                $hashGroups[$hash].Newest = $_.LastWriteTime
            }
        }

    $ordered = $hashGroups.GetEnumerator() | Sort-Object { $_.Value.Newest } -Descending
    Write-Log ("检测到 deps 哈希组: {0} 组（保留最新 {1} 组）" -f $hashGroups.Count, $Keep)
    $index = 0
    foreach ($entry in $ordered) {
        $index++
        if ($index -le $Keep) { continue }
        if (Test-BuildBusy) { exit 0 }
        $files = @($entry.Value.Files)
        $bytes = [int64]0
        foreach ($f in $files) { $bytes += [int64]$f.Length }
        $tag = $entry.Key.Substring(0, [Math]::Min(8, $entry.Key.Length))
        if (-not $Apply) {
            Write-Log ("DRY-RUN 将删除 deps 哈希组 {0}: {1} 个文件, {2:N1} MB（超出最新 {3} 代）" -f $tag, $files.Count, ($bytes / 1MB), $Keep)
            continue
        }
        $ok = 0
        foreach ($f in $files) {
            try {
                $full = [System.IO.Path]::GetFullPath($f.FullName)
                if (-not $full.StartsWith([System.IO.Path]::GetFullPath($targetRoot), [System.StringComparison]::OrdinalIgnoreCase)) {
                    Write-Log "拒绝删除 target 之外的路径: $full"
                    continue
                }
                Remove-Item -LiteralPath $full -Force -Confirm:$false -ErrorAction Stop
                $ok++
            } catch {
                Write-Log ("删除失败: {0} -> {1}" -f $f.FullName, $_.Exception.Message)
            }
        }
        $freed += $bytes
        Write-Log ("已删除 deps 哈希组 {0}: {1}/{2} 文件, {3:N1} MB（逻辑大小）" -f $tag, $ok, $files.Count, ($bytes / 1MB))
    }
}

# 2) incremental：每个 crate 前缀只保留最新 Keep 个 s-* 会话目录。
if (Test-Path $incremental) {
    $sessions = @()
    Get-ChildItem -LiteralPath $incremental -Directory -Force -ErrorAction SilentlyContinue |
        ForEach-Object {
            $prefix = $_.Name -replace '-[^-]+$', ''
            if ($prefix -eq $_.Name) { $prefix = $_.Name }
            Get-ChildItem -LiteralPath $_.FullName -Directory -Force -ErrorAction SilentlyContinue |
                Where-Object { $_.Name -like 's-*' } |
                ForEach-Object {
                    $sessions += [pscustomobject]@{
                        Prefix = $prefix
                        Session = $_
                        Top = $_
                    }
                }
        }

    $byPrefix = $sessions | Group-Object Prefix
    Write-Log ("检测到 incremental: {0} 个会话，{1} 个 crate 前缀（每个前缀保留最新 {2} 个）" -f $sessions.Count, @($byPrefix).Count, $Keep)
    foreach ($group in $byPrefix) {
        $orderedSessions = $group.Group | Sort-Object { $_.Session.LastWriteTime } -Descending
        $index = 0
        foreach ($s in $orderedSessions) {
            $index++
            if ($index -le $Keep) { continue }
            if (Test-BuildBusy) { exit 0 }
            $freed += Remove-Tree $s.Session.FullName ("incremental/{0} 会话超出最新 {1} 个" -f $group.Name, $Keep)
        }
    }

    # 会话被删空的顶层 crate 配置目录一并清掉（保留仍有 s-* 的目录）。
    Get-ChildItem -LiteralPath $incremental -Directory -Force -ErrorAction SilentlyContinue |
        ForEach-Object {
            $left = @(Get-ChildItem -LiteralPath $_.FullName -Directory -Force -ErrorAction SilentlyContinue |
                Where-Object { $_.Name -like 's-*' })
            if ($left.Count -eq 0) {
                $freed += Remove-Tree $_.FullName "incremental 空配置目录"
            }
        }
}

# 3) 旧 exe 备份：保留最新 Keep 个。
if (Test-Path $debugRoot) {
    $backups = Get-ChildItem -LiteralPath $debugRoot -File -Force -ErrorAction SilentlyContinue |
        Where-Object {
            $_.Name -like 'vpn-ctl.exe.old*' -or
            $_.Name -like 'vpn-ctl.pre-*.exe' -or
            $_.Name -like 'vpn-ctl.new-*.exe'
        } | Sort-Object LastWriteTime -Descending
    Write-Log ("检测到旧 exe 备份: {0} 个（保留最新 {1} 个）" -f $backups.Count, $Keep)
    $index = 0
    foreach ($b in $backups) {
        $index++
        if ($index -le $Keep) { continue }
        if (Test-BuildBusy) { exit 0 }
        $freed += Remove-Tree $b.FullName ("旧 exe 备份超出最新 {0} 个" -f $Keep)
    }
}

if ($Apply) {
    $freeAfter = [int64]0
    try { $freeAfter = (Get-Volume -DriveLetter $volumeLetter).SizeRemaining } catch { }
    $delta = $freeAfter - $freeBefore
    Write-Log ("完成：逻辑删除 {0:N1} MB，磁盘可用变化 {1:N1} MB（硬链接的实际释放以这个为准）" -f ($freed / 1MB), ($delta / 1MB))
} else {
    Write-Log "dry-run 完成，未删除任何文件；确认后加 -Apply 执行"
}
