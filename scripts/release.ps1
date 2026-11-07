# Ships a release: verifies what the Release workflow will check, then pushes the branch.
# GitHub Actions (.github/workflows/release.yml) sees a Cargo.toml version without a tag on main,
# builds the three systems' files, creates the tag vX.Y.Z and publishes the release. No local tag is needed.
#
#   1. scripts\bump-version.ps1 [patch|minor|major|x.y.z]   (bumps Cargo.toml + Cargo.lock, adds a CHANGELOG entry)
#   2. describe the release in CHANGELOG.md, commit it        ("Release vX.Y.Z: ...")
#   3. scripts\release.ps1                                    (this script; or just push main from GitHub Desktop)
param([switch]$NoPush)
$ErrorActionPreference = "Continue"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
trap { Write-Host "`nERROR: $_" -ForegroundColor Red; exit 1 }

# git may not be on PATH (GitHub Desktop ships its own copy)
$git = (Get-Command git -ErrorAction SilentlyContinue).Source
if (-not $git) {
    $git = Get-ChildItem "$env:LOCALAPPDATA\GitHubDesktop" -Recurse -Filter git.exe -ErrorAction SilentlyContinue |
        Where-Object FullName -like '*\cmd\git.exe' | Sort-Object LastWriteTime -Descending | Select-Object -First 1 -ExpandProperty FullName
}
if (-not $git) { throw "git not found (install Git or GitHub Desktop)" }

$version = ([regex]'(?m)^version\s*=\s*"([^"]+)"').Match([IO.File]::ReadAllText("$root\Cargo.toml")).Groups[1].Value
$tag = "v$version"

# release notes must exist and be filled in (the workflow uses them as the release body)
& "$PSScriptRoot\release-notes.ps1" -Version $version | Out-Null

if (& $git status --porcelain) { throw "working tree has uncommitted changes - commit them first" }
$branch = (& $git rev-parse --abbrev-ref HEAD).Trim()
if ($branch -ne "main") { throw "releases are published from main (you are on $branch)" }
& $git ls-remote --exit-code --tags origin "refs/tags/$tag" | Out-Null
if ($LASTEXITCODE -eq 0) { throw "$tag is already released - bump the version first" }

if ($NoPush) {
    Write-Host "Ready: $tag on main. Push main (git push origin main, or Push origin in GitHub Desktop) to release it." -ForegroundColor Yellow
    exit 0
}
Write-Host "Pushing main ($((& $git rev-parse --short HEAD).Trim())) - the Release workflow will tag and publish $tag" -ForegroundColor Cyan
& $git push origin main
if ($LASTEXITCODE -ne 0) { throw "git push failed - push main from GitHub Desktop instead; the workflow does the rest" }
Write-Host "`nPushed. The Release workflow is now building ${tag}:" -ForegroundColor Green
Write-Host "  https://github.com/Akulav/local-anime/actions"
