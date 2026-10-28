#!/usr/bin/env bash
# Prints the body for a GitHub release: download links for this build, how to open it, then the generated changelog.
# Usage: release-notes.sh <tag> <repo> <commit-sha> <ci-run-url> <dir-with-installers>
# Needs the gh CLI (for the changelog). Used by the release job in .github/workflows/ci.yml.
set -euo pipefail
tag=$1 repo=$2 sha=$3 run_url=$4 dist=$5
base="https://github.com/$repo"

dmg=$(cd "$dist" && ls ./*.dmg 2>/dev/null | head -1 | sed 's|^\./||' || true)
exe=$(cd "$dist" && ls ./*.exe 2>/dev/null | head -1 | sed 's|^\./||' || true)

echo "## Download"
echo
echo "| Platform | File |"
echo "|---|---|"
[ -n "$dmg" ] && echo "| macOS, Apple silicon | [$dmg]($base/releases/download/$tag/$dmg) |"
[ -n "$exe" ] && echo "| Windows 10/11, 64-bit | [$exe]($base/releases/download/$tag/$exe) |"
echo
echo "Built from commit [\`${sha:0:7}\`]($base/commit/$sha) in [this CI run]($run_url)."
echo
echo "**Opening the app the first time.** The Mac build is ad hoc signed, not notarized: right-click Backer in Applications, choose Open, then Open again. The Windows installer is unsigned: if SmartScreen warns, choose More info, then Run anyway."
echo
gh api -X POST "repos/$repo/releases/generate-notes" -f tag_name="$tag" -f target_commitish="$sha" --jq .body
