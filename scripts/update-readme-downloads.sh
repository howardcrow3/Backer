#!/usr/bin/env bash
# Rewrites the "latest build" block of README.md (between the latest-build markers) to point at a release.
# Usage: update-readme-downloads.sh <tag> <repo> <dir-with-installers> [readme] [date]
# Run by the release job in .github/workflows/ci.yml after it publishes a release.
set -euo pipefail
tag=$1 repo=$2 dist=$3 readme=${4:-README.md} date=${5:-$(date -u +%Y-%m-%d)}
base="https://github.com/$repo"
dmg=$(cd "$dist" && ls ./*.dmg 2>/dev/null | head -1 | sed 's|^\./||' || true)
exe=$(cd "$dist" && ls ./*.exe 2>/dev/null | head -1 | sed 's|^\./||' || true)

block="**Latest build: [$tag]($base/releases/tag/$tag)** ($date)

| Platform | Download |
|---|---|"
[ -n "$dmg" ] && block+="
| macOS, Apple silicon | [$dmg]($base/releases/download/$tag/$dmg) |"
[ -n "$exe" ] && block+="
| Windows 10/11, 64-bit | [$exe]($base/releases/download/$tag/$exe) |"
block+="

The Mac build is ad hoc signed, not notarized: right-click the app, choose Open, then Open again. The Windows installer is unsigned: if SmartScreen warns, choose More info, then Run anyway. Older builds are on the [releases page]($base/releases)."

BLOCK="$block" README="$readme" python3 - <<'PY'
import os, re
p = os.environ["README"]
s = open(p).read()
pat = re.compile(r"<!-- latest-build:start -->.*?<!-- latest-build:end -->", re.S)
if not pat.search(s):
    raise SystemExit("README has no latest-build markers")
open(p, "w").write(pat.sub(lambda m: "<!-- latest-build:start -->\n" + os.environ["BLOCK"] + "\n<!-- latest-build:end -->", s))
PY
