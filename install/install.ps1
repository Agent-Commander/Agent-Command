$ErrorActionPreference = 'Stop'
$base = if ($env:AGENT_COMMAND_RELEASE_URL) { $env:AGENT_COMMAND_RELEASE_URL } else { 'https://github.com/Agent-Commander/Agent-Command/releases/latest/download' }
$architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
$arch = switch ($architecture) { 'X64' { 'x86_64' } 'Arm64' { 'aarch64' } default { throw 'Supported processors: x64 and ARM64.' } }
$asset = "agent-command-$arch-pc-windows-msvc.zip"
$temp = Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid().ToString())
New-Item -ItemType Directory -Path $temp | Out-Null
try {
    Invoke-WebRequest "$base/$asset" -OutFile (Join-Path $temp $asset) -UseBasicParsing
    Invoke-WebRequest "$base/$asset.sha256" -OutFile (Join-Path $temp "$asset.sha256") -UseBasicParsing
    $expected = ((Get-Content (Join-Path $temp "$asset.sha256") -Raw).Trim() -split '\s+')[0]
    $actual = (Get-FileHash (Join-Path $temp $asset) -Algorithm SHA256).Hash
    if ($actual -ne $expected) { throw 'Download checksum mismatch; nothing installed.' }
    Expand-Archive -LiteralPath (Join-Path $temp $asset) -DestinationPath (Join-Path $temp 'unpacked')
    $bin = if ($env:AGENT_COMMAND_INSTALL_DIR) { $env:AGENT_COMMAND_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'AgentCommand\bin' }
    New-Item -ItemType Directory -Force -Path $bin | Out-Null
    $exe = Join-Path $bin 'agent-command.exe'
    $old = "$exe.old"
    if (Test-Path -LiteralPath $old) {
        try { Remove-Item -LiteralPath $old -Force -ErrorAction Stop } catch {}
    }
    $renamed = $false
    if (Test-Path -LiteralPath $exe) {
        $version = & $exe --version 2>$null
        if ($version -notmatch '^agent-command ') { throw "Refusing to replace unrelated file: $exe" }
        Move-Item -LiteralPath $exe -Destination $old -Force
        $renamed = $true
    }
    try {
        Copy-Item -LiteralPath (Join-Path $temp 'unpacked\agent-command.exe') -Destination $exe -Force
    } catch {
        if ($renamed -and (Test-Path -LiteralPath $old) -and -not (Test-Path -LiteralPath $exe)) {
            Move-Item -LiteralPath $old -Destination $exe -Force
        }
        throw
    }
    if (Test-Path -LiteralPath $old) {
        try { Remove-Item -LiteralPath $old -Force -ErrorAction Stop } catch {}
    }
    if (-not $env:AGENT_COMMAND_INSTALL_DIR) {
        $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
        if ($bin -notin ($userPath -split ';')) {
            [Environment]::SetEnvironmentVariable('Path', "$bin;$userPath", 'User')
        }
    }
    if ($bin -notin ($env:Path -split ';')) { $env:Path = "$bin;$env:Path" }
    Write-Host 'Installed Agent Command. Run: agent-command'
    if ($env:AGENT_COMMAND_NO_LAUNCH -ne '1') { & $exe }
} finally {
    Remove-Item -LiteralPath $temp -Recurse -Force
}
