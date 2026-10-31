#!/usr/bin/env pwsh
# Adapted from Deno's install script at https://github.com/denoland/deno_install/blob/HEAD/install.ps1
# All rights reserved. MIT license.

$ErrorActionPreference = 'Stop'

$Version = $args.Length -gt 0 ? $args.Get(0) : $null;
$DprintIsWindows = $IsWindows -or $PSVersionTable.PSEdition -ne 'Core'

$BinDir = switch ($env:DPRINT_INSTALL) {
	{ $_ -ne $null } { Join-Path $env:DPRINT_INSTALL "bin" }
	{ $DprintIsWindows } { Join-Path $Home ".dprint" "bin" }
	default { Join-Path $Home ".dprint" "bin" }
}
$KprintZip = Join-Path $BinDir "kprint.zip"
$KprintExe = Join-Path $BinDir "kprint.exe"
$AliasExe = Join-Path $BinDir "dprint.exe"

# use the OS architecture (not the process arch) so the native build is chosen
# even when running under x64 emulation on Windows on ARM
$Target = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -eq [System.Runtime.InteropServices.Architecture]::Arm64 ? 'aarch64-pc-windows-msvc' : 'x86_64-pc-windows-msvc'

# GitHub requires TLS 1.2
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

# Resolve the permanent repository ID so downloads survive repository renames.
$RepositoryHeaders = @{}
if ($env:GITHUB_TOKEN) { $RepositoryHeaders.Authorization = "Bearer $env:GITHUB_TOKEN" }
$Repository = Invoke-RestMethod 'https://api.github.com/repositories/1092062077' -UseBasicParsing -Headers $RepositoryHeaders
$RepositoryUrl = $Repository.html_url
$ReleaseUri = $Version ? "$RepositoryUrl/releases/download/$Version" : "$RepositoryUrl/releases/latest/download"

if (!(Test-Path $BinDir)) { New-Item $BinDir -ItemType Directory | Out-Null }

# stop any running editor services
Stop-Process -Name "kprint" -Erroraction 'silentlycontinue'
Stop-Process -Name "dprint" -Erroraction 'silentlycontinue'

# download; a release before kprint has dprint-<target>.zip with dprint.exe in it
try { Invoke-WebRequest "$ReleaseUri/kprint-${Target}.zip" -OutFile $KprintZip }
catch { Invoke-WebRequest "$ReleaseUri/dprint-${Target}.zip" -OutFile $KprintZip }

if (Test-Path $AliasExe) { Remove-Item $AliasExe }
if (Get-Command Expand-Archive -ErrorAction SilentlyContinue) { Expand-Archive $KprintZip -Destination $BinDir -Force }
else {
	if (Test-Path $KprintExe) { Remove-Item $KprintExe }
	Add-Type -AssemblyName System.IO.Compression.FileSystem
	[IO.Compression.ZipFile]::ExtractToDirectory($KprintZip, $BinDir)
}
if (Test-Path $AliasExe) { Move-Item $AliasExe $KprintExe -Force }

Remove-Item $KprintZip

# dprint.exe for what calls that, as a hard link that needs no administrator
New-Item -ItemType HardLink -Path $AliasExe -Target $KprintExe | Out-Null

$User = [EnvironmentVariableTarget]::User
$Path = [Environment]::GetEnvironmentVariable('Path', $User)
if (!(";$Path;".ToLower() -like "*;$BinDir;*".ToLower())) { [Environment]::SetEnvironmentVariable('Path', "$Path;$BinDir", $User) }
if (!(";$Env:Path;".ToLower() -like "*;$BinDir;*".ToLower())) { $Env:Path += ";$BinDir" }

Write-Output "kprint was installed successfully to $KprintExe"
Write-Output "Run 'kprint --help' to get started"
