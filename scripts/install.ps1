[CmdletBinding()]
param(
    [string]$Source = (Join-Path $PSScriptRoot '..\target\release\rusteditor.exe'),
    [string]$ProviderSource = '',
    [string]$CopilotSdkSource = (Join-Path $PSScriptRoot '..\providers\copilot')
)

$ErrorActionPreference = 'Stop'

$sourcePath = (Resolve-Path $Source).Path
$installDirectory = Join-Path $env:LOCALAPPDATA 'Programs\RustEditor'
$installedExecutable = Join-Path $installDirectory 'rusteditor.exe'

New-Item -ItemType Directory -Path $installDirectory -Force | Out-Null
Copy-Item -Path $sourcePath -Destination $installedExecutable -Force

if (-not $ProviderSource) {
    $ProviderSource = Join-Path (Split-Path $sourcePath) 'rusteditor-openai.exe'
}
if (Test-Path $ProviderSource) {
    Copy-Item -Path $ProviderSource -Destination (Join-Path $installDirectory 'rusteditor-openai.exe') -Force
    Write-Host 'Installed the provider helper (OpenAI, Anthropic, Copilot).'
} else {
    Write-Warning 'Provider helper not found; build with cargo build --release -p rusteditor-openai to enable provider setup.'
}

# The SDK is an optional separate Node provider, never linked into the editor.
if (Test-Path (Join-Path $CopilotSdkSource 'package.json')) {
    if (Get-Command npm.cmd -ErrorAction SilentlyContinue) {
        Push-Location $CopilotSdkSource
        try {
            & npm.cmd ci
            if ($LASTEXITCODE -ne 0) { throw 'Copilot SDK dependency installation failed.' }
            & npm.cmd run build
            if ($LASTEXITCODE -ne 0) { throw 'Copilot SDK provider build failed.' }
        } finally { Pop-Location }
        $sdkDirectory = Join-Path $installDirectory 'copilot-sdk'
        New-Item -ItemType Directory -Path $sdkDirectory -Force | Out-Null
        Copy-Item (Join-Path $CopilotSdkSource 'dist') $sdkDirectory -Recurse -Force
        Copy-Item (Join-Path $CopilotSdkSource 'node_modules') $sdkDirectory -Recurse -Force
        Copy-Item (Join-Path $CopilotSdkSource 'package.json') $sdkDirectory -Force
        Write-Host 'Installed the official GitHub Copilot SDK provider.'
    } else {
        Write-Warning 'Node.js/npm not found: Copilot SDK is not installed. OpenAI and Anthropic still work.'
    }
}

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
