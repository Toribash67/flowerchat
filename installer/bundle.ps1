# Gathers flowerchat and the parts of GStreamer it uses into one folder that
# runs on a machine with nothing else installed:
#
#   bin\                    the programs and the libraries they load
#   lib\gstreamer-1.0\      the GStreamer plugins
#   libexec\gstreamer-1.0\  GStreamer's helper for reading plugins
#
# GStreamer looks for its plugins beside the folder its own library is in,
# which is why the programs are in bin\ and not at the top.
#
# Run after `cargo build --release`.
param(
    # Where GStreamer is installed.
    [string]$GStreamer = "$env:LOCALAPPDATA\Programs\gstreamer\1.0\msvc_x86_64",
    # Where cargo put the programs.
    [string]$Programs = "$PSScriptRoot\..\target\release",
    # The folder to make. It is emptied first.
    [string]$Out = "$PSScriptRoot\..\target\bundle"
)
$ErrorActionPreference = 'Stop'

# The plugins behind every element the pipelines name, and behind the ones
# webrtcbin builds by itself (dtls, srtp, sctp). When a pipeline gains an
# element from another plugin, it goes here; `gst-inspect-1.0 ELEMENT` says
# which plugin that is.
$plugins = @(
    'app', 'audioconvert', 'audiomixer', 'audioresample', 'audiotestsrc',
    'coreelements', 'd3d11', 'debug', 'dtls', 'level', 'nice', 'nvcodec',
    'opus', 'rtp', 'rtpmanager', 'sctp', 'srtp', 'videoconvertscale',
    'videoparsersbad', 'videorate', 'videotestsrc', 'volume', 'wasapi2',
    'webrtc', 'webrtcdsp'
)

# The names of the libraries a program or library asks Windows to load with
# it, read from its import table.
function Get-Imports([string]$Path) {
    $bytes = [IO.File]::ReadAllBytes($Path)
    $pe = [BitConverter]::ToInt32($bytes, 0x3C)
    $sections = [BitConverter]::ToUInt16($bytes, $pe + 6)
    $optional = $pe + 24
    $table = $optional + [BitConverter]::ToUInt16($bytes, $pe + 20)
    # 64-bit programs keep their directories 112 bytes in, 32-bit ones 96.
    $directories = $optional + $(if ([BitConverter]::ToUInt16($bytes, $optional) -eq 0x20B) { 112 } else { 96 })
    $imports = [BitConverter]::ToUInt32($bytes, $directories + 8)
    if ($imports -eq 0) { return }
    # Addresses in the file are given as they will be in memory.
    $offset = {
        param($address)
        for ($i = 0; $i -lt $sections; $i++) {
            $section = $table + 40 * $i
            $start = [BitConverter]::ToUInt32($bytes, $section + 12)
            $size = [Math]::Max([BitConverter]::ToUInt32($bytes, $section + 8), [BitConverter]::ToUInt32($bytes, $section + 16))
            if ($address -ge $start -and $address -lt $start + $size) {
                return $address - $start + [BitConverter]::ToUInt32($bytes, $section + 20)
            }
        }
        throw "$Path has an address outside its sections"
    }
    for ($entry = & $offset $imports; ; $entry += 20) {
        $name = [BitConverter]::ToUInt32($bytes, $entry + 12)
        if ($name -eq 0) { break }
        $at = & $offset $name
        $end = [Array]::IndexOf($bytes, [byte]0, $at)
        [Text.Encoding]::ASCII.GetString($bytes, $at, $end - $at)
    }
}

if (Test-Path $Out) { Remove-Item -Recurse -Force $Out }
$bin = New-Item -ItemType Directory -Force "$Out\bin"
$lib = New-Item -ItemType Directory -Force "$Out\lib\gstreamer-1.0"
$libexec = New-Item -ItemType Directory -Force "$Out\libexec\gstreamer-1.0"

$placed = @()
foreach ($program in 'flowerchat.exe', 'flowerchat-cli.exe') {
    $placed += Copy-Item "$Programs\$program" $bin -PassThru
}
foreach ($plugin in $plugins) {
    $placed += Copy-Item "$GStreamer\lib\gstreamer-1.0\gst$plugin.dll" $lib -PassThru
}
$placed += Copy-Item "$GStreamer\libexec\gstreamer-1.0\gst-plugin-scanner.exe" $libexec -PassThru

# Everything placed so far needs libraries, which need libraries. Those that
# are GStreamer's come along; the rest are parts of Windows.
$waiting = [Collections.Generic.Queue[string]]::new([string[]]$placed.FullName)
while ($waiting.Count -gt 0) {
    foreach ($name in Get-Imports $waiting.Dequeue()) {
        $source = "$GStreamer\bin\$name"
        if ((Test-Path $source) -and -not (Test-Path "$bin\$name")) {
            $waiting.Enqueue((Copy-Item $source $bin -PassThru).FullName)
        }
    }
}

Copy-Item "$PSScriptRoot\THIRD-PARTY.txt" $Out

$files = Get-ChildItem -Recurse -File $Out
'{0} files, {1:N0} MB in {2}' -f $files.Count, (($files | Measure-Object Length -Sum).Sum / 1MB), (Resolve-Path $Out)
