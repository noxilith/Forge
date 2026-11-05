#!/bin/sh
# Build Forge in release mode and install the `forge` command.
#   ./install.sh                 -> ~/.local/bin/forge
#   PREFIX=/usr/local ./install.sh   (needs write access to $PREFIX/bin)
set -eu
cd "$(dirname "$0")"

# Some systems have gcc but no `cc`, which Rust's default linker setup expects.
if ! command -v cc >/dev/null 2>&1 && command -v gcc >/dev/null 2>&1; then
    export CC="${CC:-gcc}"
    export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="${CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER:-gcc}"
fi

cargo build --release -p forge-cli

dest="${PREFIX:-$HOME/.local}/bin"
mkdir -p "$dest"
# Replace atomically so a running forge isn't disturbed.
install -m 0755 target/release/forge "$dest/forge.new"
mv -f "$dest/forge.new" "$dest/forge"
echo "Installed $dest/forge"

case ":$PATH:" in
    *":$dest:"*) ;;
    *) echo "Note: $dest is not on your PATH." ;;
esac
echo "Next: forge doctor"
