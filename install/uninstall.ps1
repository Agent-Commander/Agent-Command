$ErrorActionPreference = 'Stop'
$appDir = Join-Path $env:LOCALAPPDATA 'AgentCommand'
$bin = if ($env:AGENT_COMMAND_INSTALL_DIR) { $env:AGENT_COMMAND_INSTALL_DIR } else { Join-Path $appDir 'bin' }
$exe = Join-Path $bin 'agent-command.exe'
$authDir = if ($env:SUB_AUTH_DIR) { $env:SUB_AUTH_DIR } else { Join-Path $env:USERPROFILE '.local\share\sub-auth' }

function Get-ThemePath {
    if ($env:AGENT_COMMAND_THEME) { return $env:AGENT_COMMAND_THEME }
    if ($env:XDG_CONFIG_HOME) { return Join-Path $env:XDG_CONFIG_HOME 'agent-command\theme.toml' }
    if ($env:APPDATA) { return Join-Path $env:APPDATA 'agent-command\theme.toml' }
    return Join-Path $env:USERPROFILE '.config\agent-command\theme.toml'
}

function Get-AgentCommandVersion([string]$path) {
    try { return [string](& $path --version 2>$null | Select-Object -First 1) } catch { return '' }
}

function Remove-AgentCommandExe {
    if (-not (Test-Path -LiteralPath $exe)) { Write-Host "No agent-command.exe found in $bin"; return }
    if ((Get-AgentCommandVersion $exe) -notmatch '^agent-command ') { throw "Refusing to remove unrelated file: $exe" }
    try { Remove-Item -LiteralPath $exe -Force }
    catch { throw "Could not remove $exe because it is locked, running, or read-only. Close every Agent Command window and run the uninstaller again." }
    Write-Host "Removed $exe"
}

function Remove-EmptyDirectory([string]$path) {
    if (-not (Test-Path -LiteralPath $path -PathType Container)) { return }
    if (Get-ChildItem -LiteralPath $path -Force | Select-Object -First 1) { return }
    Remove-Item -LiteralPath $path -Force
    Write-Host "Removed empty $path"
}

function Remove-UserPathEntry([string]$entry) {
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not $userPath) { return }
    $entries = $userPath -split ';'
    if ($entry -notin $entries) { return }
    $kept = @($entries | Where-Object { $_ -ne $entry })
    [Environment]::SetEnvironmentVariable('Path', ($kept -join ';'), 'User')
    Write-Host "Removed $entry from your user Path"
}

function Test-CargoInstall {
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { return $false }
    try { return [bool](& cargo install --list 2>$null | Where-Object { $_ -match '^agent-command ' }) } catch { return $false }
}

Remove-AgentCommandExe
Remove-EmptyDirectory $bin
if (-not $env:AGENT_COMMAND_INSTALL_DIR) {
    Remove-EmptyDirectory $appDir
    Remove-UserPathEntry $bin
}
$env:Path = (@($env:Path -split ';' | Where-Object { $_ -ne $bin }) -join ';')
Write-Host "Kept sign-ins and app data: $authDir"
Write-Host "Kept theme settings: $(Get-ThemePath)"
Write-Host 'To sign out, run agent-command auth logout PROVIDER PROFILE first; reinstall to do it now.'
if (Test-CargoInstall) { Write-Host 'Cargo also installed agent-command. Next step: cargo uninstall agent-command' }
Write-Host 'Uninstalled Agent Command.'
