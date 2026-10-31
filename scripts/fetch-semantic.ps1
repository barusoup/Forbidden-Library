# Fetch the pieces semantic search needs, once, into vendor/semantic.
#
#   powershell -ExecutionPolicy Bypass -File scripts/fetch-semantic.ps1
#
# Everything is pinned by URL and SHA-256 so a build is reproducible and a
# corrupt download is never bundled. Existing verified files are reused;
# package.ps1 calls this automatically, so installers never download anything.

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"
$root = Split-Path -Parent $PSScriptRoot
$base = Join-Path $root "vendor\semantic"
$models = Join-Path $base "model"
$runtimes = Join-Path $base "runtime"
New-Item -ItemType Directory -Force -Path $models, $runtimes | Out-Null

# Get-FileHash is not available in every PowerShell host; use the framework
# directly so the script behaves the same everywhere.
function Get-Sha256 {
    param([string]$Path)
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        $stream = [System.IO.File]::OpenRead($Path)
        try {
            return ([BitConverter]::ToString($sha.ComputeHash($stream))).Replace("-", "").ToLowerInvariant()
        } finally { $stream.Dispose() }
    } finally { $sha.Dispose() }
}

function Get-Pinned {
    param([string]$Url, [string]$Destination, [string]$Sha256)
    if (Test-Path $Destination) {
        $existing = Get-Sha256 $Destination
        if ($existing -eq $Sha256) {
            Write-Host "  present  $(Split-Path -Leaf $Destination)"
            return
        }
        Write-Host "  replacing $(Split-Path -Leaf $Destination) (checksum mismatch)"
        Remove-Item -LiteralPath $Destination -Force
    }
    Write-Host "  download $(Split-Path -Leaf $Destination)"
    $partial = "$Destination.part"
    if (Test-Path $partial) { Remove-Item -LiteralPath $partial -Force }
    Invoke-WebRequest -Uri $Url -OutFile $partial -UseBasicParsing
    $actual = Get-Sha256 $partial
    if ($actual -ne $Sha256) {
        Remove-Item -LiteralPath $partial -Force
        throw "Checksum mismatch for $Url (expected $Sha256, got $actual)"
    }
    Move-Item -LiteralPath $partial -Destination $Destination -Force
}

Write-Host "Semantic model (EmbeddingGemma 2, Unsloth UD-Q4_K_XL + Q8 projector)"
Get-Pinned `
    "https://huggingface.co/unsloth/embeddinggemma-2-GGUF/resolve/main/embeddinggemma-2-UD-Q4_K_XL.gguf?download=true" `
    (Join-Path $models "embeddinggemma-2-UD-Q4_K_XL.gguf") `
    "ea905fd08e8061db77a0031cbb5096d459d7cfd2bdd0ad2ba66e483f16719493"
Get-Pinned `
    "https://huggingface.co/unsloth/embeddinggemma-2-GGUF/resolve/main/mmproj-Q8_0.gguf?download=true" `
    (Join-Path $models "mmproj-Q8_0.gguf") `
    "90e7b0238009e2954f856f2081dcf7f35af026b64c765e98f4777053e1754460"

# llama.cpp b11846: Vulkan is used when a GPU is available, the CPU build is
# the fallback. Both are self-contained next to llama-server.exe.
$build = "b11846"
$runtimePins = @(
    @{ Name = "vulkan"; Url = "https://github.com/ggml-org/llama.cpp/releases/download/$build/llama-$build-bin-win-vulkan-x64.zip"; Sha = "e2f44c4e16e1db544716ecdce8f86d3ce9f92e1c8d8f5b8e5efc5a3e0757c3b2" },
    @{ Name = "cpu";    Url = "https://github.com/ggml-org/llama.cpp/releases/download/$build/llama-$build-bin-win-cpu-x64.zip";    Sha = "d4eeebd157d93085ef64b56bb35e4c8771d14ce1b0f15a97b982b3f42a25bb9d" }
)

Write-Host "llama.cpp $build runtime"
foreach ($runtime in $runtimePins) {
    $zip = Join-Path $runtimes "$($runtime.Name).zip"
    Get-Pinned $runtime.Url $zip $runtime.Sha
    $target = Join-Path $runtimes $runtime.Name
    if (-not (Test-Path (Join-Path $target "llama-server.exe"))) {
        if (Test-Path $target) { Remove-Item -Recurse -Force $target }
        New-Item -ItemType Directory -Force -Path $target | Out-Null
        Expand-Archive -LiteralPath $zip -DestinationPath $target -Force
    }
}

$size = (Get-ChildItem -Recurse -File $base | Measure-Object -Property Length -Sum).Sum
Write-Host ("Semantic assets ready: {0:N0} MB in {1}" -f ($size / 1MB), $base)
