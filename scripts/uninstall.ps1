[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'

$installDirectory = Join-Path $env:LOCALAPPDATA 'Programs\RustEditor'
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$newUserPath = @($userPath -split ';' | Where-Object {
    $_ -and $_.TrimEnd('\') -ine $installDirectory.TrimEnd('\')
}) -join ';'
[Environment]::SetEnvironmentVariable('Path', $newUserPath, 'User')

$classes = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey(
    'Software\Classes',
    $true
)
try {
    $classes.DeleteSubKeyTree('*\shell\RustEditor', $false)
}
finally {
    $classes.Dispose()
}

if (Test-Path $installDirectory) {
    Remove-Item -Path $installDirectory -Recurse -Force
}
Write-Host 'Removed RustEditor from the current user PATH and file context menu.'
