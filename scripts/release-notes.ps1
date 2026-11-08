# Prints the CHANGELOG.md section for one version (used as the GitHub release body).
#   scripts\release-notes.ps1 4.1.0            -> writes to stdout
#   scripts\release-notes.ps1 4.1.0 notes.md   -> writes to a file
# Fails if the version has no section or the section is still the empty template from bump-version.
param(
    [Parameter(Mandatory = $true)][string]$Version,
    [string]$OutFile
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$changelog = [IO.File]::ReadAllText((Join-Path $root "CHANGELOG.md"))

$v = [regex]::Escape($Version.TrimStart("v"))
$m = [regex]::Match($changelog, "(?ms)^## \[$v\][^\r\n]*\r?\n(.*?)(?=^## \[|\z)")
if (-not $m.Success) { throw "CHANGELOG.md has no section for version $Version" }

$body = $m.Groups[1].Value.Trim()
# a section containing only headings / empty bullets means nobody wrote the notes yet
$content = ($body -split "`n" | Where-Object { $_.Trim() -notmatch '^(#+ .*|-\s*)?$' })
if (-not $content) { throw "CHANGELOG.md section for $Version is empty - describe the release first" }

$body += "`n`n---`n" +
    "| System | Download | |`n|---|---|---|`n" +
    "| **Windows** 10/11 x64 | ``Tsukimi-setup.exe`` | setup wizard, per user, no admin rights |`n" +
    "| **macOS** Apple Silicon | ``tsukimi-macos-arm64`` | ``chmod +x tsukimi-macos-arm64`` and run it: it installs itself (~/Applications) |`n" +
    "| **Linux** x64 | ``tsukimi-linux-x64`` | ``chmod +x tsukimi-linux-x64`` and run it: it installs itself (app menu) |`n`n" +
    "Already have Tsukimi? Use **Update** in the app (sidebar or Settings). Every file is listed in ``SHA256SUMS.txt``.`n" +
    "Coming from dango? Install Tsukimi over it: your library, progress and downloads move over on the first start.`n`n" +
    "Made by Akulav, inspired by dango by serifpersia."
if ($OutFile) { [IO.File]::WriteAllText($OutFile, $body) } else { $body }
