[CmdletBinding()]
param(
    [string]$Source = (Join-Path $PSScriptRoot '..\target\release\rusteditor.exe')
)

$ErrorActionPreference = 'Stop'

$sourcePath = (Resolve-Path $Source).Path
$installDirectory = Join-Path $env:LOCALAPPDATA 'Programs\RustEditor'
$installedExecutable = Join-Path $installDirectory 'rusteditor.exe'

New-Item -ItemType Directory -Path $installDirectory -Force | Out-Null
Copy-Item -Path $sourcePath -Destination $installedExecutable -Force

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$pathEntries = @($userPath -split ';' | Where-Object { $_ })
$isOnPath = $pathEntries | Where-Object {
    $_.TrimEnd('\') -ieq $installDirectory.TrimEnd('\')
}

if (-not $isOnPath) {
    $newUserPath = ($pathEntries + $installDirectory) -join ';'
    [Environment]::SetEnvironmentVariable('Path', $newUserPath, 'User')
}

$classes = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey(
    'Software\Classes',
    $true
)
$menuKey = $classes.CreateSubKey('*\shell\RustEditor')
$commandKey = $menuKey.CreateSubKey('command')
try {
    $menuKey.SetValue('', 'Open with RustEditor')
    $menuKey.SetValue('Icon', "`"$installedExecutable`"")
    $commandKey.SetValue('', ('"{0}" "%1"' -f $installedExecutable))
}
finally {
    $commandKey.Dispose()
    $menuKey.Dispose()
    $classes.Dispose()
}

Write-Host "Installed RustEditor to $installedExecutable"
Write-Host 'Added RustEditor to the current user PATH.'
Write-Host 'Added "Open with RustEditor" to the file context menu.'
Write-Host 'Open a new terminal before running rusteditor by name.'
