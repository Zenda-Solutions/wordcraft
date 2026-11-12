<#
.SYNOPSIS
  Build Zenda's WordCraft for Work and pack the portable zip Work downloads.
.DESCRIPTION
  Builds wordcraft.exe (with the zenda feature) and wordcraft-cli.exe, and writes
  dist\zenda\wordcraft-<version>-z<n>-windows-x64-portable.zip. Inside, the folder keeps upstream's
  name (wordcraft-<version>-windows-x64-portable), so Work's manifest keeps the same `exe` path.
  Prints the zip's size and SHA-256 for app/manifest.json in zenda-work.
  Set CRAFT_I18N_DIR to a craft-i18n checkout at craft-i18n.rev with zenda/i18n.ps1 applied.
.EXAMPLE
  pwsh zenda/package.ps1 -Z 1
#>
param([Parameter(Mandatory)] [int] $Z, [switch] $SkipBuild)
$ErrorActionPreference = 'Stop'
$Root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$Version = (Select-String -Path (Join-Path $Root 'Cargo.toml') -Pattern '^version = "([^"]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
if (-not $env:WORDCRAFT_BUILD_SHA) { $env:WORDCRAFT_BUILD_SHA = (git -C $Root rev-parse HEAD) }
if (-not $SkipBuild) {
  $env:RUSTFLAGS = '-C target-feature=+crt-static'
  cargo build --release --locked -p wordcraft -p wordcraft-cli --features wordcraft/zenda
  if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
}
$Target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $Root 'target' }
$Dist = Join-Path $Root 'dist\zenda'
$Folder = "wordcraft-$Version-windows-x64-portable"
$Stage = Join-Path $Dist $Folder
Remove-Item -Recurse -Force $Dist -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $Stage | Out-Null
Copy-Item (Join-Path $Target 'release\wordcraft.exe'), (Join-Path $Target 'release\wordcraft-cli.exe') $Stage
foreach ($f in 'LICENSE-APACHE', 'LICENSE-MIT', 'NOTICE', 'ATTRIBUTION.md') { Copy-Item (Join-Path $Root $f) $Stage }
foreach ($ofl in Get-ChildItem (Join-Path $Root 'assets\fonts') -Filter 'OFL-*.txt') { Copy-Item $ofl.FullName $Stage }
$Zip = Join-Path $Dist "wordcraft-$Version-z$Z-windows-x64-portable.zip"
Compress-Archive -Path $Stage -DestinationPath $Zip
$item = Get-Item $Zip
$sha = (Get-FileHash -Algorithm SHA256 $Zip).Hash.ToLower()
Write-Output "zip:    $($item.Name)"
Write-Output "size:   $($item.Length)"
Write-Output "sha256: $sha"
Write-Output "exe:    $Folder/wordcraft.exe"
