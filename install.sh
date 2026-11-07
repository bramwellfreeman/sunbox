#!/bin/sh
# Sunbox installer: curl -fsSL https://github.com/bramwellfreeman/sunbox/releases/latest/download/install.sh | sh
set -e
case "$(uname -s)" in
  Darwin) asset=sunbox-macos-universal.tar.gz ;;
  Linux)
    [ "$(uname -m)" = x86_64 ] || { echo "Sunbox has Linux builds for x86_64 only — build from source with cargo."; exit 1; }
    asset=sunbox-linux-x86_64.tar.gz ;;
  *) echo "Sunbox runs on macOS and Linux."; exit 1 ;;
esac
dest="${SUNBOX_BIN:-$HOME/.local/bin}"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
echo "Downloading Sunbox…"
curl -fsSL "https://github.com/bramwellfreeman/sunbox/releases/latest/download/$asset" | tar -xz -C "$tmp"
mkdir -p "$dest"
install -m 755 "$tmp/sunbox" "$dest/sunbox"
echo "✓ Installed $dest/sunbox"

case ":$PATH:" in
  *":$dest:"*) ;;
  *)
    # macOS zsh reads ~/.zprofile for login shells; most Linux shells read ~/.profile.
    rc="$HOME/.profile"
    [ "$(uname -s)" = Darwin ] && rc="$HOME/.zprofile"
    echo "export PATH=\"$dest:\$PATH\"" >> "$rc"
    echo "  Added $dest to your PATH in $rc — open a new terminal window."
    ;;
esac
echo
echo "Next: sunbox auth setup   (paste the Google client secret)"
echo "      sunbox auth login   (sign in to Gmail in your browser)"
echo "      sunbox              (open your inbox)"
