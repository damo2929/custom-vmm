#!/usr/bin/env bash
# Build a local sysroot for the C libraries this tree links, without root.
#
# The supported way to satisfy HOST-REQUIREMENTS.md §6 is to install the
# -devel packages system-wide:
#
#   sudo dnf install -y ffmpeg-devel libva-devel libvorbis-devel \
#                       librados-devel librbd-devel x264-devel
#
# This script is the fallback for machines where that is not possible. It
# downloads the same RPMs into a scratch directory, unpacks the headers and
# the unversioned linker symlinks, and rewrites the pkg-config files to point
# at them. The runtime .so.N libraries still come from the host: only the
# development-time bits are staged here.
#
# Usage: scripts/setup-local-sysroot.sh [<prefix>]
#   Writes <prefix>/{rpms,sysroot} and emits .cargo/config.toml in the repo.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PREFIX="${1:-${VMM_SYSROOT_PREFIX:-$REPO/.sysroot}}"
SYSROOT="$PREFIX/sysroot"
RPMS="$PREFIX/rpms"

# The -devel packages named in HOST-REQUIREMENTS.md §6, plus the transitive
# -devel packages their pkg-config files declare a Requires: on.
PACKAGES=(
    ffmpeg-devel        # libavcodec/libavutil/libswscale - H.264 encode+decode, scaling
    libva-devel         # VA-API - hardware H.264 encode (§7.1)
    x264-devel          # software H.264 encode fallback (§7.1)
    libvorbis-devel     # Vorbis encode (§7.1)
    libogg-devel        # Vorbis' container dependency
    librados-devel      # RADOS - rust_ceph_rbd engine (§5.4)
    librbd-devel        # RBD images and native snapshots (§10.2)
)

need() { command -v "$1" >/dev/null || { echo "error: $1 is required" >&2; exit 1; }; }
need dnf; need rpm2cpio; need cpio

echo "==> staging into $PREFIX"
mkdir -p "$RPMS" "$SYSROOT"

echo "==> downloading ${#PACKAGES[@]} packages and their dependencies"
dnf download --resolve --alldeps --destdir "$RPMS" "${PACKAGES[@]}" >/dev/null

# RPMs carry their installed modes, and the `filesystem` package ships the
# standard directories read-only (/usr/lib64 is 0555). cpio then cannot write
# into them, so every -devel package unpacked afterwards silently loses
# whatever landed there — which is how libva, libvorbis, libogg and x264 came
# to be missing their unversioned .so symlinks while this script still
# reported success. Reopening the directories once is not enough either: any
# later package that ships them re-applies the read-only mode. So reopen the
# few we actually read from before every package.
#
# cpio's exit status is deliberately ignored. `--alldeps` drags in the whole
# base-OS closure, and packages like findutils genuinely fail to unpack here
# (they write to /usr/bin, which we neither need nor keep writable). What
# matters is not that every package extracted but that the result links,
# which the verify step below now checks directly.
echo "==> unpacking (x86_64 only; the i686 multilib copies would collide)"
reopen_dirs() {
    chmod u+wx "$SYSROOT/usr" "$SYSROOT/usr/lib" "$SYSROOT/usr/lib64" \
               "$SYSROOT/usr/lib64/pkgconfig" "$SYSROOT/usr/include" 2>/dev/null || true
}

count=0
for rpm in "$RPMS"/*.rpm; do
    case "$(basename "$rpm")" in
        *.i686.rpm) continue ;;   # multilib copy; would collide with x86_64
    esac
    reopen_dirs
    ( cd "$SYSROOT" && rpm2cpio "$rpm" | cpio -idmu --quiet ) 2>/dev/null || true
    count=$((count + 1))
done
echo "    unpacked $count packages"

# The symlink and pkg-config rewrites below need to write into the tree too.
chmod -R u+w "$SYSROOT"

# The unversioned .so symlinks an RPM ships are relative and point at a
# .so.N that only exists on the host. Repoint them at the host's copy so the
# linker resolves them, and leave the runtime loader using the host as well.
echo "==> repointing linker symlinks at /usr/lib64"
fixed=0
for link in "$SYSROOT"/usr/lib64/*.so; do
    [ -L "$link" ] || continue
    target="$(readlink "$link")"
    if [ -e "/usr/lib64/$target" ]; then
        ln -sf "/usr/lib64/$target" "$link"
        fixed=$((fixed + 1))
    fi
done
echo "    repointed $fixed symlinks"

# Fedora installs the FFmpeg headers under a ffmpeg/ subdirectory, so its
# pkg-config files carry a different includedir from everything else.
echo "==> rewriting pkg-config files"
for pc in "$SYSROOT"/usr/lib64/pkgconfig/*.pc; do
    include="$SYSROOT/usr/include"
    case "$(basename "$pc")" in
        libav*|libsw*) include="$SYSROOT/usr/include/ffmpeg" ;;
    esac
    sed -i -e "s|^libdir=.*|libdir=$SYSROOT/usr/lib64|" \
           -e "s|^includedir=.*|includedir=$include|" "$pc"
done

echo "==> verifying"
export PKG_CONFIG_PATH="$SYSROOT/usr/lib64/pkgconfig"
missing=0
for mod in libavcodec libavutil libswscale libva libva-drm vorbisenc ogg x264; do
    if version="$(pkg-config --modversion "$mod" 2>/dev/null)"; then
        printf '    %-12s %s\n' "$mod" "$version"
    else
        printf '    %-12s MISSING\n' "$mod"; missing=$((missing + 1))
    fi
done
# Ceph ships no pkg-config files, so the sys crate links it by name instead.
for header in rados/librados.h rbd/librbd.h; do
    if [ -e "$SYSROOT/usr/include/$header" ]; then
        printf '    %-12s %s\n' "$(dirname "$header")" "$header"
    else
        printf '    %-12s MISSING (%s)\n' "$(dirname "$header")" "$header"; missing=$((missing + 1))
    fi
done
[ "$missing" -eq 0 ] || { echo "error: $missing dependencies unresolved" >&2; exit 1; }

# pkg-config answering is not the same as the sysroot being usable: a .pc
# file resolves from headers alone, while the linker needs the unversioned
# .so symlink beside it. Checking only the former is what let a sysroot
# missing six of those symlinks report success. Link something.
if command -v cc >/dev/null; then
    probe="$(mktemp -d)"
    trap 'rm -rf "$probe"' EXIT
    echo 'int main(void){return 0;}' > "$probe/link-test.c"
    if cc "$probe/link-test.c" -o "$probe/link-test" \
         $(pkg-config --cflags --libs libavcodec libavutil libswscale libva libva-drm vorbisenc ogg x264) \
         2> "$probe/err"; then
        echo "    link test         ok"
    else
        echo "error: the sysroot resolves with pkg-config but does not link:" >&2
        sed 's/^/    /' "$probe/err" >&2
        exit 1
    fi
else
    echo "    link test         skipped (no cc)"
fi

# .cargo/config.toml is checked in — it carries the x86-64-v3 baseline that
# every machine needs. Only the trailing [env] stanza is machine-local, so
# rewrite that and leave the rest of the file alone.
mkdir -p "$REPO/.cargo"
CONFIG="$REPO/.cargo/config.toml"
if [ -f "$CONFIG" ]; then
    # Drop any existing [env] section (it runs to end of file) and re-add it.
    sed -i '/^\[env\]$/,$d' "$CONFIG"
    # Collapse the trailing blank lines sed may have left behind.
    printf '%s\n' "$(cat "$CONFIG")" > "$CONFIG"
else
    printf '[build]\nrustflags = ["-C", "target-cpu=x86-64-v3"]\n' > "$CONFIG"
fi
cat >> "$CONFIG" <<EOF

# Machine-local, rewritten by scripts/setup-local-sysroot.sh. Do not commit a
# path from your own machine here.
[env]
VMM_SYSROOT = "$SYSROOT"
EOF

echo "==> patched $CONFIG (VMM_SYSROOT=$SYSROOT)"
echo "    cargo build will now find the C libraries."
