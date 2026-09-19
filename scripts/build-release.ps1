# Builds the Windows one-click release into .\dist
#
#   powershell -ExecutionPolicy Bypass -File scripts\build-release.ps1
#
# Produces dist\rottingapple.exe (double-click = setup wizard + auto mirror)
# plus the runtime files it needs: openh264 DLL and fpsap-helper.exe.

$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot
Set-Location $Root

$Dist = Join-Path $Root "dist"
$DllName = "openh264-2.6.0-win64.dll"
$DllUrl = "http://ciscobinary.openh264.org/openh264-2.6.0-win64.dll.bz2"
$VendorDll = Join-Path $Root "vendor\$DllName"

Write-Host "== Building rottingapple.exe (official OpenH264 DLL encoder) =="
cargo build --release -p rotten-app --no-default-features --features encode-dll
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

New-Item -ItemType Directory -Force -Path $Dist | Out-Null
Copy-Item "target\release\rottingapple.exe" $Dist -Force
Copy-Item "target\release\rottingapple-probe.exe" $Dist -Force -ErrorAction SilentlyContinue

Write-Host "== Locating $DllName =="
$dllCandidates = @($VendorDll, (Join-Path $Dist $DllName), (Join-Path $Root $DllName))
$dll = $dllCandidates | Where-Object { Test-Path $_ } | Select-Object -First 1

if (-not $dll) {
    Write-Host "Not found locally; downloading from Cisco..."
    $bz2 = Join-Path $env:TEMP "openh264.dll.bz2"
    curl.exe -fsSL -o $bz2 $DllUrl
    if ($LASTEXITCODE -ne 0) { throw "download failed: $DllUrl" }

    New-Item -ItemType Directory -Force -Path (Split-Path $VendorDll) | Out-Null
    $extracted = $false

    $sevenZip = "C:\Program Files\7-Zip\7z.exe"
    if (Test-Path $sevenZip) {
        & $sevenZip e -y -o"$(Split-Path $VendorDll)" $bz2 "$DllName" | Out-Null
        $extracted = Test-Path $VendorDll
    }
    if (-not $extracted -and (Get-Command python -ErrorAction SilentlyContinue)) {
        python -c "import bz2,sys; open(sys.argv[2],'wb').write(bz2.open(sys.argv[1],'rb').read())" $bz2 $VendorDll
        $extracted = Test-Path $VendorDll
    }
    if (-not $extracted -and (Get-Command wsl -ErrorAction SilentlyContinue)) {
        $wslPath = wsl wslpath -a "$bz2" 2>$null
        $wslOut = wsl wslpath -a "$VendorDll" 2>$null
        if ($wslPath -and $wslOut) {
            wsl bash -lc "bunzip2 -c '$wslPath' > '$wslOut'"
            $extracted = Test-Path $VendorDll
        }
    }

    if (-not $extracted) {
        Write-Host "Automatic extraction failed (no 7-Zip, Python or WSL available)."
        Write-Host "Download $DllUrl, decompress it and place $DllName in vendor\ or dist\,"
        Write-Host "then run this script again."
        exit 1
    }
    $dll = $VendorDll
}
$destDll = Join-Path $Dist $DllName
if ([System.IO.Path]::GetFullPath($dll) -ne [System.IO.Path]::GetFullPath($destDll)) {
    Copy-Item $dll $destDll -Force
}

if (Test-Path "target\release\fpsap-helper.exe") {
    Copy-Item "target\release\fpsap-helper.exe" $Dist -Force
} elseif (Test-Path "dist\fpsap-helper.exe") {
    Write-Host "fpsap-helper.exe already present in dist\"
} elseif (Get-Command go -ErrorAction SilentlyContinue) {
    Write-Host "== Building fpsap-helper.exe =="
    Push-Location "tools\fpsap-helper"
    $env:GOOS = "windows"; $env:GOARCH = "amd64"; $env:CGO_ENABLED = "0"
    go build -o "$Dist\fpsap-helper.exe" .
    Pop-Location
} else {
    Write-Host "Warning: go not found; fpsap-helper.exe not built (only needed for FairPlay Apple TVs)."
}

Write-Host ""
Write-Host "Release ready in: $Dist"
Get-ChildItem $Dist | Select-Object Name, @{n = "Size"; e = { "{0:N0}" -f $_.Length } } | Format-Table
