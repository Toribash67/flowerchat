# Builds the Windows installer from scratch: the programs, the folder they
# run from, and the installer around it. The result is
# target\installer\flowerchat-VERSION-setup.exe.
#
# Needs Rust, GStreamer 1.28 (MSVC) and Inno Setup 6; docs/windows-port.md
# says how each was installed.
param(
    # Where GStreamer is installed.
    [string]$GStreamer = "$env:LOCALAPPDATA\Programs\gstreamer\1.0\msvc_x86_64"
)
$ErrorActionPreference = 'Stop'
$root = Resolve-Path "$PSScriptRoot\.."

$env:PATH = "$GStreamer\bin;$env:PATH"
$env:PKG_CONFIG_PATH = "$GStreamer\lib\pkgconfig"

cargo build --release --locked --manifest-path "$root\Cargo.toml" -p flowerchat-app -p flowerchat-client
if ($LASTEXITCODE -ne 0) { throw 'the build failed' }

& "$PSScriptRoot\bundle.ps1" -GStreamer $GStreamer

# The installer carries the version the app has in its Cargo.toml.
$metadata = cargo metadata --no-deps --format-version 1 --manifest-path "$root\Cargo.toml" | ConvertFrom-Json
$version = ($metadata.packages | Where-Object name -eq 'flowerchat-app').version

$iscc = @(
    (Get-Command iscc -ErrorAction SilentlyContinue).Source,
    "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe",
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe"
) | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
if (-not $iscc) { throw 'Inno Setup 6 is not installed' }
& $iscc /Q "/DVersion=$version" "$PSScriptRoot\flowerchat.iss"
if ($LASTEXITCODE -ne 0) { throw 'the installer could not be made' }

Get-Item "$root\target\installer\flowerchat-$version-setup.exe" |
    ForEach-Object { '{0} ({1:N0} MB)' -f $_.FullName, ($_.Length / 1MB) }
