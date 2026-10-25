#Requires -Version 5.1
<#
    build-engine.ps1 —— 编译 Rust 常驻引擎。

    用法:
        powershell -NoProfile -ExecutionPolicy Bypass -File scripts\build-engine.ps1
        powershell -NoProfile -ExecutionPolicy Bypass -File scripts\build-engine.ps1 -Profile debug
        powershell -NoProfile -ExecutionPolicy Bypass -File scripts\build-engine.ps1 -Target x86_64-pc-windows-gnu

    说明:
        本机没有 MSVC 链接器(未装 VS Build Tools),所以装的是 GNU 宿主的工具链
        (默认 host = x86_64-pc-windows-gnu),链接器用 C:\mingw64 的 gcc。
        既然默认宿主已经是 GNU,**日常不需要传 -Target**;只有确实要交叉编译到
        别的 triple 时才传,那时产物会落在 target\<triple>\<profile>\。

    脚本自己处理两件事,不用手工铺环境:
        1. gcc 不在 PATH 时,从常见 MinGW 目录临时补进本次进程的 PATH;
        2. cargo 的 warning 走 stderr,而 PS 5.1 在 $ErrorActionPreference='Stop'
           下会把原生命令的 stderr 当成致命错误 —— 结果是第一条 warning 就把
           脚本打断。这里把 stderr 降级成普通输出,成败只认 cargo 的退出码。

    退出码:0 成功;2 缺工具链或缺 gcc;3 编译失败;4 产物缺失。
#>
[CmdletBinding()]
param(
    [ValidateSet('release', 'debug')]
    [string]$Profile = 'release',

    [string]$Target = ''
)

$ErrorActionPreference = 'Stop'

$pluginRoot = Split-Path -Parent $PSScriptRoot
$engineDir = Join-Path $pluginRoot 'rust-engine'

if (-not (Test-Path $engineDir)) {
    Write-Error "找不到引擎目录: $engineDir"
    exit 3
}

$cargo = Get-Command cargo -ErrorAction SilentlyContinue
if (-not $cargo) {
    Write-Host '[build-engine] 找不到 cargo —— Rust 工具链未安装。' -ForegroundColor Yellow
    Write-Host '  1) 下载 GNU 版安装器 https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-gnu/rustup-init.exe'
    Write-Host '     (下完先验一下:大小约 14.0 MB、开头两个字节是 MZ;153 字节的是 404 网页)'
    Write-Host '  2) 国内直连不稳时先设镜像 $env:RUSTUP_DIST_SERVER="https://rsproxy.cn"'
    Write-Host '  3) rustup-init.exe -y --profile minimal --default-host x86_64-pc-windows-gnu --default-toolchain stable-x86_64-pc-windows-gnu'
    Write-Host '  4) 重开终端后重新运行本脚本'
    exit 2
}

# windows-gnu 目标靠 gcc 当链接器。少了它,cargo 会在最后一步报
# "linker `gcc` not found",而那行错通常被几百行 warning 淹掉、极难定位,
# 所以在开编之前先把它验掉。
$isGnu = $true
if ($Target) {
    $isGnu = $Target -like '*-gnu'
}
else {
    # 用 2>&1 | Out-String 而不是 2>$null:同样是躲开 Stop 模式下的 stderr 陷阱。
    $hostDump = ''
    try { $hostDump = (& rustc -vV 2>&1 | Out-String) } catch { $hostDump = '' }
    if ($hostDump -match 'host:\s*(\S+)') { $isGnu = $Matches[1] -like '*-gnu' }
}

if ($isGnu -and -not (Get-Command gcc -ErrorAction SilentlyContinue)) {
    foreach ($cand in @('C:\mingw64\bin', 'C:\msys64\mingw64\bin', 'C:\ProgramData\mingw64\mingw64\bin', 'C:\mingw32\bin', 'C:\mingw\bin')) {
        if (Test-Path (Join-Path $cand 'gcc.exe')) {
            $env:PATH = "$cand;$env:PATH"
            Write-Host "[build-engine] gcc 不在 PATH,已临时补上: $cand" -ForegroundColor Yellow
            break
        }
    }
}
if ($isGnu -and -not (Get-Command gcc -ErrorAction SilentlyContinue)) {
    Write-Host '[build-engine] 找不到 gcc —— windows-gnu 目标必须有一个链接器。' -ForegroundColor Red
    Write-Host '  装个 MinGW-w64,或把它的 bin 目录加进 PATH 后重跑本脚本。'
    exit 2
}

$cargoArgs = @('build')
if ($Profile -eq 'release') { $cargoArgs += '--release' }
if ($Target) { $cargoArgs += @('--target', $Target) }

Write-Host "[build-engine] cargo $($cargoArgs -join ' ')  (目录: $engineDir)"

Push-Location $engineDir
try {
    # 见文件头第 2 条。这里用 Start-Process 而不是 `& cargo`:
    # PS 5.1 调用原生命令时,只要它往 stderr 写东西就会生成 ErrorRecord,
    # 而 ErrorRecord 带着 "所在位置 <脚本>:101 字符:9" 那样的位置信息和
    # "NativeCommandError" 字样打到控制台 —— 看起来像脚本炸了,其实只是 warning。
    # `2>$file` 只把文本写进文件,ErrorRecord 本身照样往外吐。
    # Start-Process 的 -RedirectStandardOutput/-RedirectStandardError 走的是纯
    # Win32 重定向,根本不经过 PowerShell 的错误流,输出干净。
    # 代价:构建期间看不到实时进度,所以下面先打一行提示。
    $outFile = Join-Path $env:TEMP 'build-engine-stdout.log'
    $errFile = Join-Path $env:TEMP 'build-engine-stderr.log'
    foreach ($log in @($outFile, $errFile)) {
        if (Test-Path $log) { Remove-Item -LiteralPath $log -Force -ErrorAction SilentlyContinue }
    }

    Write-Host '[build-engine] 编译中(输出在结束时一次性回放)...'
    $proc = Start-Process -FilePath $cargo.Source -ArgumentList $cargoArgs -NoNewWindow -Wait -PassThru `
        -RedirectStandardOutput $outFile -RedirectStandardError $errFile
    $code = $proc.ExitCode

    foreach ($log in @($outFile, $errFile)) {
        if (Test-Path $log) {
            Get-Content -LiteralPath $log | ForEach-Object { Write-Host $_ }
            Remove-Item -LiteralPath $log -Force -ErrorAction SilentlyContinue
        }
    }
}
finally {
    Pop-Location
}

if ($code -ne 0) {
    Write-Host "[build-engine] 编译失败,退出码 $code" -ForegroundColor Red
    exit 3
}

# 产物路径:cargo 带 --target 时会多一层 target\<triple>\<profile>\
$profileDir = if ($Profile -eq 'release') { 'release' } else { 'debug' }
if ($Target) {
    $exePath = Join-Path $engineDir "target\$Target\$profileDir\screen-control-engine.exe"
}
else {
    $exePath = Join-Path $engineDir "target\$profileDir\screen-control-engine.exe"
}

# 独立回读验证:脚本自己去磁盘上确认产物真的存在,而不是相信 cargo 的退出码。
if (-not (Test-Path $exePath)) {
    Write-Host "[build-engine] cargo 报告成功,但产物不存在: $exePath" -ForegroundColor Red
    Write-Host '[build-engine] 这不是"应该没问题",而是实打实的失败,请检查 Cargo.toml 的 [[bin]] name。'
    exit 4
}

# 再往 <插件根>\bin\ 固定放一份。原因:带 -Target 时产物在 target\<triple>\ 下,
# 而 TS 桥的候选路径(src/bridge.ts 的 engineCandidates)只认 target\release\、
# target\debug\ 和 bin\,不去 triple 目录里找。这一步保证"编完一定找得到"。
$binDir = Join-Path $pluginRoot 'bin'
if (-not (Test-Path $binDir)) { New-Item -ItemType Directory -Path $binDir -Force | Out-Null }
Copy-Item -LiteralPath $exePath -Destination (Join-Path $binDir 'screen-control-engine.exe') -Force

$file = Get-Item $exePath
Write-Host ("[build-engine] 成功: {0}  ({1:N0} 字节, {2})" -f $file.FullName, $file.Length, $file.LastWriteTime)
Write-Host ("[build-engine] 已同步一份到: {0}" -f (Join-Path $binDir 'screen-control-engine.exe'))
Write-Host '[build-engine] 独立验证: npm run selfcheck   (阶段二 B 会真的拉起这个二进制)'
