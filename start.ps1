if (Get-Command python -ErrorAction SilentlyContinue) {
    & python "$PSScriptRoot\run.py" @args
} elseif (Get-Command py -ErrorAction SilentlyContinue) {
    & py -3 "$PSScriptRoot\run.py" @args
} else {
    throw 'Install Python 3.10 or newer, then run start.ps1 again.'
}
exit $LASTEXITCODE
