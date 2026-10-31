# Build Forbidden Library and produce the release files in dist/:
#
#   ForbiddenLibrarySetup-<version>.exe   Inno Setup installer (first install)
#   update.zip, update.json               what installed copies download to update
#
#   powershell -ExecutionPolicy Bypass -File scripts/package.ps1
#
# Needs Inno Setup 6 (https://jrsoftware.org/isinfo.php) for ISCC.exe.

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$versionLine = Get-Content "Cargo.toml" | Where-Object { $_ -match '^version\s*=\s*"(.+)"' } | Select-Object -First 1
if (-not $versionLine) { throw "Could not read the workspace version from Cargo.toml" }
$version = [regex]::Match($versionLine, '"(.+)"').Groups[1].Value
Write-Host "Packaging Forbidden Library $version"

# FFmpeg is a separate program, not linked into the app. Keep the exact
# binaries and their licence beside the app so the install is reproducible.
$assets = Join-Path $root "assets"
$ffmpegDir = Join-Path $assets "ffmpeg"
if (-not (Test-Path (Join-Path $ffmpegDir "ffmpeg.exe")) -or
    -not (Test-Path (Join-Path $ffmpegDir "ffprobe.exe"))) {
    Write-Host "Downloading the FFmpeg essentials build..."
    $zip = Join-Path $env:TEMP "forbiddenlibrary-ffmpeg.zip"
    Invoke-WebRequest -Uri "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip" -OutFile $zip
    $extract = Join-Path $env:TEMP "forbiddenlibrary-ffmpeg-extract"
    if (Test-Path $extract) { Remove-Item -Recurse -Force $extract }
    Expand-Archive -Path $zip -DestinationPath $extract -Force
    $bin = Get-ChildItem -Path $extract -Recurse -Filter "ffmpeg.exe" | Select-Object -First 1
    if (-not $bin) { throw "The FFmpeg archive did not contain ffmpeg.exe" }
    $source = Split-Path -Parent $bin.FullName
    New-Item -ItemType Directory -Force -Path $ffmpegDir | Out-Null
    Copy-Item (Join-Path $source "ffmpeg.exe") $ffmpegDir -Force
    Copy-Item (Join-Path $source "ffprobe.exe") $ffmpegDir -Force
    $licence = Get-ChildItem -Path $extract -Recurse -Filter "LICENSE" | Select-Object -First 1
    if ($licence) { Copy-Item $licence.FullName (Join-Path $ffmpegDir "LICENSE.txt") -Force }
}

# JPEG XL is written and read back by libjxl's own command-line tools. They are
# pinned by the hash of the release archive, so a changed download cannot slip in.
$jxlDir = Join-Path $assets "jxl"
$jxlArchiveSha256 = "3025d7e308390796d20492322e606bc92decaee7b6bc99d3f7547870ae5db7de"
if (-not (Test-Path (Join-Path $jxlDir "cjxl.exe")) -or
    -not (Test-Path (Join-Path $jxlDir "djxl.exe"))) {
    Write-Host "Downloading libjxl v0.12.0..."
    $zip = Join-Path $env:TEMP "forbiddenlibrary-libjxl.zip"
    Invoke-WebRequest -Uri "https://github.com/libjxl/libjxl/releases/download/v0.12.0/jxl-x64-windows-static.zip" -OutFile $zip
    $actualSha256 = (Get-FileHash -Algorithm SHA256 -Path $zip).Hash
    if ($actualSha256 -ne $jxlArchiveSha256) {
        Remove-Item -Force $zip
        throw "The libjxl archive has SHA-256 $actualSha256, expected $jxlArchiveSha256"
    }
    $extract = Join-Path $env:TEMP "forbiddenlibrary-libjxl-extract"
    if (Test-Path $extract) { Remove-Item -Recurse -Force $extract }
    Expand-Archive -Path $zip -DestinationPath $extract -Force
    # The archive nests its tools under a platform folder (x64-windows-static\bin).
    $bin = Get-ChildItem -Path $extract -Recurse -Filter "cjxl.exe" | Select-Object -First 1
    if (-not $bin) { throw "The libjxl archive did not contain cjxl.exe" }
    $source = Split-Path -Parent $bin.FullName
    New-Item -ItemType Directory -Force -Path $jxlDir | Out-Null
    Copy-Item (Join-Path $source "cjxl.exe") $jxlDir -Force
    Copy-Item (Join-Path $source "djxl.exe") $jxlDir -Force
    Invoke-WebRequest -Uri "https://raw.githubusercontent.com/libjxl/libjxl/v0.12.0/LICENSE" -OutFile (Join-Path $jxlDir "LICENSE.txt")
}

# Semantic search's model and runtime are pinned and fetched once, then
# shipped inside the setup payload: an install never touches the network.
& (Join-Path $PSScriptRoot "fetch-semantic.ps1")
$semantic = Join-Path $root "vendor\semantic"

Write-Host "Building the app..."
cargo build --release -p forbidden-library --no-default-features
if ($LASTEXITCODE -ne 0) { throw "The app build failed" }

$dist = Join-Path $root "dist"
$payloadDir = Join-Path $dist "payload"
if (Test-Path $payloadDir) { Remove-Item -Recurse -Force $payloadDir }
New-Item -ItemType Directory -Force -Path $payloadDir | Out-Null

Copy-Item "target\release\forbidden-library.exe" (Join-Path $payloadDir "forbidden-library.exe") -Force
New-Item -ItemType Directory -Force -Path (Join-Path $payloadDir "ffmpeg") | Out-Null
Copy-Item (Join-Path $ffmpegDir "ffmpeg.exe") (Join-Path $payloadDir "ffmpeg") -Force
Copy-Item (Join-Path $ffmpegDir "ffprobe.exe") (Join-Path $payloadDir "ffmpeg") -Force
if (Test-Path (Join-Path $ffmpegDir "LICENSE.txt")) {
    Copy-Item (Join-Path $ffmpegDir "LICENSE.txt") (Join-Path $payloadDir "ffmpeg") -Force
}
New-Item -ItemType Directory -Force -Path (Join-Path $payloadDir "jxl") | Out-Null
Copy-Item (Join-Path $jxlDir "cjxl.exe") (Join-Path $payloadDir "jxl") -Force
Copy-Item (Join-Path $jxlDir "djxl.exe") (Join-Path $payloadDir "jxl") -Force
Copy-Item (Join-Path $jxlDir "LICENSE.txt") (Join-Path $payloadDir "jxl") -Force
Copy-Item "README.md" $payloadDir -Force
Copy-Item "THIRD_PARTY.md" $payloadDir -Force
Copy-Item (Join-Path $assets "fonts\LICENSE.txt") (Join-Path $payloadDir "Roboto-LICENSE.txt") -Force

# Only what llama-server needs: the executable, its backend DLLs and the
# licences that come with the release. Bench and CLI tools stay out.
New-Item -ItemType Directory -Force -Path (Join-Path $payloadDir "semantic\model") | Out-Null
Get-ChildItem (Join-Path $semantic "model") -Filter "*.gguf" | ForEach-Object {
    Copy-Item $_.FullName (Join-Path $payloadDir "semantic\model") -Force
}
foreach ($runtime in @("vulkan", "cpu")) {
    $source = Join-Path $semantic "runtime\$runtime"
    $target = Join-Path $payloadDir "semantic\runtime\$runtime"
    New-Item -ItemType Directory -Force -Path $target | Out-Null
    Copy-Item (Join-Path $source "llama-server.exe") $target -Force
    Copy-Item (Join-Path $source "*.dll") $target -Force
    Get-ChildItem $source -Filter "LICENSE*" | ForEach-Object {
        Copy-Item $_.FullName $target -Force
    }
}

# Installed copies update from a small archive: everything except the large,
# rarely changing parts (FFmpeg and the search model), which only the installer carries.
$updateZip = Join-Path $dist "update.zip"
if (Test-Path $updateZip) { Remove-Item -Force $updateZip }
$updateItems = Get-ChildItem $payloadDir | Where-Object { $_.Name -notin @("ffmpeg", "semantic") }
Compress-Archive -Path $updateItems.FullName -DestinationPath $updateZip -CompressionLevel Optimal

$updateHash = (Get-FileHash -Algorithm SHA256 -Path $updateZip).Hash.ToLower()
$manifest = [ordered]@{
    version = $version
    url     = "https://github.com/barusoup/Forbidden-Library/releases/download/v$version/update.zip"
    sha256  = $updateHash
    size    = (Get-Item $updateZip).Length
}
$manifest | ConvertTo-Json | Set-Content -Path (Join-Path $dist "update.json") -Encoding ascii

$iscc = @(
    (Get-Command ISCC.exe -ErrorAction SilentlyContinue).Source,
    "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe",
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
    "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
) | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
if (-not $iscc) { throw "Inno Setup 6 (ISCC.exe) was not found" }

Write-Host "Building the installer..."
& $iscc "/DAppVersion=$version" "/O$dist" (Join-Path $root "installer\ForbiddenLibrary.iss")
if ($LASTEXITCODE -ne 0) { throw "The installer build failed" }

$setup = Join-Path $dist "ForbiddenLibrarySetup-$version.exe"
Write-Host ""
Write-Host ("Built {0} ({1} MB)" -f $setup, [math]::Round((Get-Item $setup).Length / 1MB, 1))
Write-Host ("Update archive: {0} MB" -f [math]::Round((Get-Item $updateZip).Length / 1MB, 1))
