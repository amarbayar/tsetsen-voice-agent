#!/usr/bin/env bash
# Installs agent-desktop (Apache-2.0), which Tsetsen uses to operate apps
# through the accessibility tree. Pinned and checksum-verified. macOS on Apple
# silicon only for now.
set -euo pipefail

version=1.0.2
dir="$HOME/.tsetsen/tools/agent-desktop-$version"
file="agent-desktop-v$version-aarch64-apple-darwin.tar.gz"
base="https://github.com/vercel-labs/agent-desktop/releases/download/v$version"

if [[ -x "$dir/agent-desktop" ]]; then
  echo "Already installed: $dir/agent-desktop"
  exit 0
fi
mkdir -p "$dir"
cd "$dir"
curl -fsSLO "$base/$file"
curl -fsSLO "$base/checksums.txt"
grep " $file\$" checksums.txt | shasum -a 256 -c
tar xzf "$file"
rm "$file"
echo "Installed: $dir/agent-desktop"
echo "Now allow your terminal app under Accessibility:"
./agent-desktop permissions --guide
