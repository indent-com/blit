$ErrorActionPreference = "Stop"

function Find-RequiredCommand([string]$Name, [string]$InstallHint) {
    $Command = Get-Command $Name -CommandType Application -ErrorAction SilentlyContinue
    if (-not $Command) {
        throw "$Name was not found. $InstallHint"
    }
    return $Command.Source
}

function Invoke-Native([string]$Description, [string]$FilePath, [string[]]$ArgumentList) {
    & $FilePath @ArgumentList
    if ($LASTEXITCODE -ne 0) {
        throw "$Description failed with exit code $LASTEXITCODE."
    }
}

$RepoRoot = $PSScriptRoot
$CargoCommand = Get-Command cargo -CommandType Application -ErrorAction SilentlyContinue
if ($CargoCommand) {
    $CargoPath = $CargoCommand.Source
} else {
    $CargoPath = Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe"
    if (-not (Test-Path -LiteralPath $CargoPath)) {
        throw "cargo was not found on PATH or at $CargoPath"
    }
}
$RustcPath = Join-Path (Split-Path -Parent $CargoPath) "rustc.exe"
if (-not (Test-Path -LiteralPath $RustcPath)) {
    throw "rustc was not found beside Cargo at $RustcPath"
}
$RustupPath = Find-RequiredCommand "rustup" "Install Rust through rustup."
$NodePath = Find-RequiredCommand "node" "Install Node.js 22 or newer."
$PnpmPath = Find-RequiredCommand "pnpm" "Install pnpm 10 (for example: npm install --global pnpm@10)."
$WasmPackPath = Find-RequiredCommand "wasm-pack" "Install wasm-pack (for example: cargo install wasm-pack)."
$InstalledRustTargets = & $RustupPath target list --installed
if ($LASTEXITCODE -ne 0) {
    throw "Could not list installed Rust targets."
}
if ($InstalledRustTargets -notcontains "wasm32-unknown-unknown") {
    throw "Rust target wasm32-unknown-unknown is required. Run: rustup target add wasm32-unknown-unknown"
}
$RustHost = (& $RustcPath -vV | Where-Object { $_ -like "host: *" }) -replace "^host: ", ""
$MsvcArch = switch ($RustHost) {
    "aarch64-pc-windows-msvc" { "arm64" }
    "x86_64-pc-windows-msvc" { "x64" }
    "i686-pc-windows-msvc" { "x86" }
    default { throw "unsupported MSVC Rust host target: $RustHost" }
}

$VsWhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
if (-not (Test-Path -LiteralPath $VsWhere)) {
    throw "Visual Studio's vswhere.exe was not found at $VsWhere"
}
$VsInstall = & $VsWhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $VsInstall) {
    throw "Visual Studio with the x64 MSVC build tools is required"
}
$VsDevCmd = Join-Path $VsInstall "Common7\Tools\VsDevCmd.bat"
if (-not (Test-Path -LiteralPath $VsDevCmd)) {
    throw "Visual Studio developer command prompt was not found at $VsDevCmd"
}
$ClangDir = Join-Path $VsInstall "VC\Tools\Llvm\$MsvcArch\bin"
if (-not (Test-Path -LiteralPath (Join-Path $ClangDir "clang.exe"))) {
    throw "Visual Studio's $MsvcArch clang.exe was not found at $ClangDir"
}

# Import the matching MSVC environment into this PowerShell process so Cargo can
# find the linker and Windows SDK regardless of how this shell was opened.
$VsEnvironment = & cmd.exe /d /s /c "call `"$VsDevCmd`" -arch=$MsvcArch -host_arch=$MsvcArch >nul && set"
foreach ($Line in $VsEnvironment) {
    if ($Line -match "^([^=]+)=(.*)$") {
        Set-Item -LiteralPath "Env:$($matches[1])" -Value $matches[2]
    }
}
$env:PATH = "$ClangDir;$env:PATH"
Set-Item -LiteralPath "Env:CC_$($RustHost -replace '-', '_')" -Value (Join-Path $ClangDir "clang.exe")

Push-Location $RepoRoot
try {
    Push-Location (Join-Path $RepoRoot "crates\browser")
    try {
        Invoke-Native "Build browser WebAssembly" $WasmPackPath @("build", "--target", "web", "--release", "--out-dir", "pkg")
    } finally {
        Pop-Location
    }

    Push-Location (Join-Path $RepoRoot "js")
    try {
        Invoke-Native "Install JavaScript dependencies" $PnpmPath @("install", "--frozen-lockfile")
        Invoke-Native "Build @blit-sh/core" $PnpmPath @("--filter", "@blit-sh/core", "run", "build")
        Invoke-Native "Build @blit-sh/solid" $PnpmPath @("--filter", "@blit-sh/solid", "run", "build")
        Invoke-Native "Build @blit-sh/ui" $PnpmPath @("--filter", "@blit-sh/ui", "run", "build")
    } finally {
        Pop-Location
    }

    Invoke-Native "Build blit" $CargoPath @("build", "-p", "blit-cli", "--profile", "profiling")
} finally {
    Pop-Location
}
