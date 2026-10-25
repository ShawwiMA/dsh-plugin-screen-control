#Requires -Version 5.1
<#
    link-into-profile.ps1 —— 把本插件挂进 dsh profile。

    默认是 dry-run,只打印将要做什么;确认无误后加 -Apply 才真正落盘。

    用法:
        powershell -NoProfile -ExecutionPolicy Bypass -File scripts\link-into-profile.ps1
        powershell -NoProfile -ExecutionPolicy Bypass -File scripts\link-into-profile.ps1 -Apply
        powershell -NoProfile -ExecutionPolicy Bypass -File scripts\link-into-profile.ps1 -Profile web -Apply

    做的事(全部可逆,改前先备份):
        1. 备份 profile 的 package.json(名字里写清是哪一步之前);
        2. 在 dependencies 加 "dsh-plugin-screen-control": "link:<本插件目录>";
        3. 在 dsh.profile.bundles 末尾追加 "dsh-plugin-screen-control";
        4. 在该 profile 目录跑 pnpm install。

    退出码:0 成功;2 目录不存在;3 落盘失败。
#>
[CmdletBinding()]
param(
    [string]$Profile = 'desktop',
    [switch]$Apply
)

$ErrorActionPreference = 'Stop'

$pluginRoot = Split-Path -Parent $PSScriptRoot
$dshHome = if ($env:DSH_HOME) { $env:DSH_HOME } else { Join-Path $env:USERPROFILE '.dsh' }
$profileDir = Join-Path $dshHome "profiles\$Profile"
$pkgPath = Join-Path $profileDir 'package.json'

if (-not (Test-Path $pkgPath)) {
    Write-Host "[link] profile 不存在: $pkgPath" -ForegroundColor Red
    exit 2
}

$pkg = Get-Content $pkgPath -Raw | ConvertFrom-Json
$linkSpec = 'link:' + ($pluginRoot -replace '\\', '/')

Write-Host "[link] profile        : $profileDir"
Write-Host "[link] 插件目录        : $pluginRoot"
Write-Host "[link] 依赖写法        : dsh-plugin-screen-control = $linkSpec"
Write-Host "[link] bundles 追加    : dsh-plugin-screen-control"

if (-not $Apply) {
    Write-Host '[link] 这是 dry-run。确认无误后加 -Apply 真正写入。' -ForegroundColor Yellow
    exit 0
}

$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$backup = Join-Path $profileDir "package.json.bak-$stamp-加screen-control前"
Copy-Item $pkgPath $backup -Force
Write-Host "[link] 已备份到 $backup"

# 用文本行处理而不是 ConvertTo-Json:profile 的 package.json 是手写文件,
# 重新序列化会打乱键顺序和缩进,以后 diff 起来一团糟。
$lines = Get-Content $pkgPath
$out = New-Object System.Collections.Generic.List[string]
$inDeps = $false
$depAdded = $false
$bundleAdded = $false

foreach ($line in $lines) {
    if ($line -match '^\s*"dependencies"\s*:\s*\{') {
        $inDeps = $true
        $out.Add($line)
        continue
    }
    if ($inDeps -and $line -match '^\s*\}') {
        if (-not $depAdded) {
            # 上一条依赖原本是最后一条,末尾没有逗号 —— 这里必须补上,否则插进去的
            # 新行会和它粘成一句非法 JSON(2026-10-16 实测踩到,脚本自己检出并回滚了)。
            if ($out.Count -gt 0 -and $out[$out.Count - 1] -notmatch ',\s*$') {
                $out[$out.Count - 1] = $out[$out.Count - 1] + ','
            }
            # 新条目插在 dependencies 的最后一条,所以自己不能带尾逗号(严格 JSON 不允许)。
            $entry = '    "dsh-plugin-screen-control": "' + $linkSpec + '"'
            $out.Add($entry)
            $depAdded = $true
        }
        $inDeps = $false
        $out.Add($line)
        continue
    }
    # bundles 数组:在 "dsh-whale-widget" 这类条目后追加,保持 JSON 合法(给上一行补逗号)
    if (-not $bundleAdded -and $line -match '^\s*"@deepseek-ai/dsh-base"\s*,?\s*$') {
        if ($line -notmatch ',\s*$') { $line = $line + ',' }
        $out.Add($line)
        $out.Add('        "dsh-plugin-screen-control",')
        $bundleAdded = $true
        continue
    }
    $out.Add($line)
}

if (-not $depAdded) {
    Write-Host '[link] 没找到 dependencies 块,放弃写入(不改坏 profile 配置)。' -ForegroundColor Red
    Copy-Item $backup $pkgPath -Force
    exit 3
}

# 落盘必须是无 BOM 的 UTF-8。PS 5.1 的 Set-Content -Encoding UTF8 会写
# EF BB BF,而 DSH 用 Node 的 JSON.parse 读这份文件 —— BOM 会让 DSH 启动
# 直接崩(2026-10-16 实测:Unexpected token '',要手工删掉 BOM 才能启动)。
# 同时保留原文件的换行风格(这份 profile 用 LF,Set-Content 会改成 CRLF)。
$rawOld = [System.IO.File]::ReadAllText($pkgPath)
$eol = if ($rawOld.Contains("`r`n")) { "`r`n" } else { "`n" }
$tail = if ($rawOld -match "(`r`n|`n)$") { $eol } else { '' }
[System.IO.File]::WriteAllText($pkgPath, (($out -join $eol) + $tail), (New-Object System.Text.UTF8Encoding($false)))

# 独立回读验证,分三道:
#   1. 头 3 字节不能是 BOM —— PS 的 Get-Content/ConvertFrom-Json 会自己吞掉
#      BOM,拿它验 BOM 等于没验(上一版就是这么漏过去的);
#   2. 交给 Node 的 JSON.parse —— 那才是 DSH 真正用的解析器;
#   3. ConvertFrom-Json 复核结构。
$bytes = [System.IO.File]::ReadAllBytes($pkgPath)
if ($bytes.Length -ge 3 -and $bytes[0] -eq 0xEF -and $bytes[1] -eq 0xBB -and $bytes[2] -eq 0xBF) {
    Write-Host '[link] 写出的文件带 BOM,正在回滚。' -ForegroundColor Red
    Copy-Item $backup $pkgPath -Force
    exit 3
}

if (Get-Command node -ErrorAction SilentlyContinue) {
    & node -e "JSON.parse(require('fs').readFileSync(process.argv[1],'utf8'))" $pkgPath
    if ($LASTEXITCODE -ne 0) {
        Write-Host '[link] Node 解析失败(DSH 用的就是这个解析器),正在回滚。' -ForegroundColor Red
        Copy-Item $backup $pkgPath -Force
        exit 3
    }
}

try {
    $check = Get-Content $pkgPath -Raw | ConvertFrom-Json
}
catch {
    Write-Host '[link] 写入后 JSON 解析失败,正在回滚。' -ForegroundColor Red
    Copy-Item $backup $pkgPath -Force
    exit 3
}

if (-not $check.dependencies.'dsh-plugin-screen-control') {
    Write-Host '[link] 回读验证失败:dependencies 里没有新条目,回滚。' -ForegroundColor Red
    Copy-Item $backup $pkgPath -Force
    exit 3
}

if (-not ($check.dsh.profile.bundles -contains 'dsh-plugin-screen-control')) {
    Write-Host '[link] 警告:bundles 里没有追加成功,请手动检查。' -ForegroundColor Yellow
}

Write-Host '[link] 写入并回读验证通过,开始 pnpm install ...'
Push-Location $profileDir
try {
    & pnpm install
    $code = $LASTEXITCODE
}
finally {
    Pop-Location
}

if ($code -ne 0) {
    Write-Host "[link] pnpm install 失败,退出码 $code。profile 配置已改,可用备份回滚: $backup" -ForegroundColor Red
    exit 3
}

Write-Host '[link] 完成。重启 dsh 后,用 win_engine_status 确认插件是否加载。' -ForegroundColor Green
