# Builds the Windows one-click release into .\dist
#
#   powershell -ExecutionPolicy Bypass -File scripts\build-release.ps1
#
# Produces dist\cermin.exe (double-click = setup wizard + auto mirror)
# plus the runtime files it needs: openh264 DLL and fpsap-helper.exe.

$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot
Set-Location $Root

$Dist = Join-Path $Root "dist"
$Target = "x86_64-pc-windows-msvc"
$TargetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $Root "target" }
$ReleaseDir = Join-Path $TargetDir "$Target\release"
$DllName = "openh264-2.6.0-win64.dll"
$DllUrl = "https://ciscobinary.openh264.org/openh264-2.6.0-win64.dll.bz2"
$VendorDll = Join-Path $Root "vendor\$DllName"

Write-Host "== Building cermin.exe (GUI) + cermin-cli.exe (official OpenH264 DLL encoder) =="
cargo build --locked --release --target $Target -p rotten-app --no-default-features --features encode-dll,gui --bins
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
cargo build --locked --release --target $Target -p rotten-probe
if ($LASTEXITCODE -ne 0) { throw "cargo build failed for cermin-probe" }

New-Item -ItemType Directory -Force -Path $Dist | Out-Null
Copy-Item (Join-Path $ReleaseDir "cermin.exe") $Dist -Force
Copy-Item (Join-Path $ReleaseDir "cermin-cli.exe") $Dist -Force
Copy-Item (Join-Path $ReleaseDir "cermin-probe.exe") $Dist -Force

Write-Host "== Locating $DllName =="
$dllCandidates = @($VendorDll, (Join-Path $Dist $DllName), (Join-Path $Root $DllName))
$dll = $dllCandidates | Where-Object { (Test-Path -LiteralPath $_ -PathType Leaf) -and (Get-Item -LiteralPath $_).Length -gt 0 } | Select-Object -First 1

if (-not $dll) {
    Write-Host "Not found locally; downloading from Cisco..."
    $DownloadDir = Join-Path $env:TEMP ("cermin-openh264-" + [guid]::NewGuid().ToString("N"))
    New-Item -ItemType Directory -Path $DownloadDir | Out-Null
    # Keep the real basename: bzip2 stores no filename, so 7-Zip uses this name.
    $bz2 = Join-Path $DownloadDir "$DllName.bz2"
    $ExtractedDll = Join-Path $DownloadDir $DllName
    try {
        curl.exe -fsSL -o $bz2 $DllUrl
        if ($LASTEXITCODE -ne 0) { throw "download failed: $DllUrl" }
        $extracted = $false
        $sevenZip = "C:\Program Files\7-Zip\7z.exe"
        if (Test-Path $sevenZip) {
            & $sevenZip e -y "-o$DownloadDir" $bz2 | Out-Null
            $extracted = ($LASTEXITCODE -eq 0) -and (Test-Path -LiteralPath $ExtractedDll)
        }
        if (-not $extracted -and (Get-Command python -ErrorAction SilentlyContinue)) {
            python -c "import bz2,pathlib,sys; pathlib.Path(sys.argv[2]).write_bytes(bz2.decompress(pathlib.Path(sys.argv[1]).read_bytes()))" $bz2 $ExtractedDll
            $extracted = ($LASTEXITCODE -eq 0) -and (Test-Path -LiteralPath $ExtractedDll)
        }
        if (-not $extracted -or (Get-Item -LiteralPath $ExtractedDll).Length -eq 0) {
            throw "Extraction failed. Install 7-Zip or Python, or decompress $DllUrl into vendor\$DllName and retry."
        }
        New-Item -ItemType Directory -Force -Path (Split-Path $VendorDll) | Out-Null
        Move-Item -LiteralPath $ExtractedDll -Destination $VendorDll -Force
    } finally {
        # Remove only the known files created by this invocation.
        Remove-Item -LiteralPath $bz2, $ExtractedDll -Force -ErrorAction SilentlyContinue
        Remove-Item -LiteralPath $DownloadDir -Force -ErrorAction SilentlyContinue
    }
    $dll = $VendorDll
}
$destDll = Join-Path $Dist $DllName
if ([System.IO.Path]::GetFullPath($dll) -ne [System.IO.Path]::GetFullPath($destDll)) {
    Copy-Item $dll $destDll -Force
}

if (Test-Path (Join-Path $ReleaseDir "fpsap-helper.exe")) {
    Copy-Item (Join-Path $ReleaseDir "fpsap-helper.exe") $Dist -Force
} elseif (Test-Path "dist\fpsap-helper.exe") {
    Write-Host "fpsap-helper.exe already present in dist\"
} elseif (Get-Command go -ErrorAction SilentlyContinue) {
    Write-Host "== Building fpsap-helper.exe =="
    Push-Location "tools\fpsap-helper"
    $PreviousGoOS, $PreviousGoArch, $PreviousCgo = $env:GOOS, $env:GOARCH, $env:CGO_ENABLED
    try {
        $env:GOOS = "windows"
        $env:GOARCH = "amd64"
        $env:CGO_ENABLED = "0"
        go build -o "$Dist\fpsap-helper.exe" .
        if ($LASTEXITCODE -ne 0) { throw "go build failed for fpsap-helper" }
    } finally {
        $env:GOOS, $env:GOARCH, $env:CGO_ENABLED = $PreviousGoOS, $PreviousGoArch, $PreviousCgo
        Pop-Location
    }
} else {
    Write-Host "Warning: go not found; fpsap-helper.exe not built (only needed for FairPlay Apple TVs)."
}

Write-Host ""
Write-Host "Release ready in: $Dist"
Get-ChildItem $Dist | Select-Object Name, @{n = "Size"; e = { "{0:N0}" -f $_.Length } } | Format-Table
