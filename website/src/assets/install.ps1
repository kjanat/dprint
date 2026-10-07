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
$DprintZip = Join-Path $BinDir "dprint.zip"
$DprintExe = Join-Path $BinDir "dprint.exe"

# use the OS architecture (not the process arch) so the native build is chosen
# even when running under x64 emulation on Windows on ARM
$Target = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -eq [System.Runtime.InteropServices.Architecture]::Arm64 ? 'aarch64-pc-windows-msvc' : 'x86_64-pc-windows-msvc'

# GitHub requires TLS 1.2
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

# Resolve the permanent repository ID so downloads survive repository renames.
$Repository = Invoke-RestMethod 'https://api.github.com/repositories/1092062077' -UseBasicParsing
$RepositoryUrl = $Repository.html_url
$DprintUri = $Version ? "$RepositoryUrl/releases/download/$Version/dprint-${Target}.zip" : "$RepositoryUrl/releases/latest/download/dprint-${Target}.zip"

if (!Test-Path $BinDir) { New-Item $BinDir -ItemType Directory | Out-Null }

# stop any running dprint editor services
Stop-Process -Name "dprint" -Erroraction 'silentlycontinue'

# download and install
Invoke-WebRequest $DprintUri -OutFile $DprintZip

if (Get-Command Expand-Archive -ErrorAction SilentlyContinue) { Expand-Archive $DprintZip -Destination $BinDir -Force }
else {
	if (Test-Path $DprintExe) { Remove-Item $DprintExe }
	Add-Type -AssemblyName System.IO.Compression.FileSystem
	[IO.Compression.ZipFile]::ExtractToDirectory($DprintZip, $BinDir)
}

Remove-Item $DprintZip

$User = [EnvironmentVariableTarget]::User
$Path = [Environment]::GetEnvironmentVariable('Path', $User)
if (!(";$Path;".ToLower() -like "*;$BinDir;*".ToLower())) { [Environment]::SetEnvironmentVariable('Path', "$Path;$BinDir", $User) }
if (!(";$Env:Path;".ToLower() -like "*;$BinDir;*".ToLower())) { $Env:Path += ";$BinDir" }

Write-Output "dprint was installed successfully to $DprintExe"
Write-Output "Run 'dprint --help' to get started"
