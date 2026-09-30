# 使い方: pwsh scripts/check-windows-exe.ps1 <tsute.exe>
# フォルダコピーで動くことの確認（ADR-0016）: Windows に標準で入っていない DLL（VC++ ランタイム・WebView2Loader）に
# 依存していたら失敗させる。desktop.yml と release.yml で共有する。
param([Parameter(Mandatory = $true)][string]$Exe)
$ErrorActionPreference = "Stop"
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$dumpbin = & $vswhere -latest -products * -find "VC\Tools\MSVC\**\bin\Hostx64\x64\dumpbin.exe" | Select-Object -First 1
$deps = & $dumpbin /nologo /dependents $Exe | Where-Object { $_ -match '^\s+\S+\.dll$' } | ForEach-Object { $_.Trim() }
if (-not $deps) { throw "could not read imports of $Exe" }
Write-Output "imports: $($deps -join ', ')"
$bad = $deps | Where-Object { $_ -match '^(vcruntime|msvcp|WebView2Loader)' }
if ($bad) { throw "$Exe depends on: $($bad -join ', ')" }
