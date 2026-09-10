#!/usr/bin/env bash
# Inventory the C libraries this build links.
#
# §1.1's blanket no-C rule was lifted (see HOST-REQUIREMENTS.md §6), so this
# no longer fails a build for linking C. It reports what is linked, and fails
# only if something arrives that is *not* on the expected list — an accidental
# C dependency is still a defect.
#
#   ./scripts/c-dependency-inventory.sh [extra cargo tree args]

set -uo pipefail

CARGO_ARGS=("$@")
[ -d "$HOME/.cargo/bin" ] && PATH="$HOME/.cargo/bin:$PATH"

# Deliberate C dependencies, and what each one is for.
declare -A EXPECTED=(
    [ring]="TLS 1.3 — §8.2, §7.4, §9.2"
    [zstd-sys]=".vmbk compression — §10.3"
    [cc]="build scripts, and the libx264 shim"
    [vmm-codec-sys]="H.264 + Vorbis encode, H.264 decode — §7.1"
    [vmm-rbd-sys]="librados/librbd — §5.4, §10.2"
    [clang-sys]="libclang, loaded by bindgen at build time only"
)

# `-sys` by name only: nothing is linked. Kept separate from EXPECTED so the
# distinction between "C we chose" and "C we only appear to have" stays
# visible. Each entry is checked with ldd, not taken on trust.
declare -A PURE_RUST_DESPITE_NAME=(
    [linux-raw-sys]="raw syscall constants, generated Rust (via rustix -> tar)"
    [wayland-sys]="wayland-client's Rust backend links nothing; its build script emits no link directive and ldd shows no libwayland"
)

unexpected=0

echo "== C-linking crates in the dependency graph =="
found=$(cargo tree --workspace --edges normal,build "${CARGO_ARGS[@]}" 2>/dev/null \
    | grep -oE '\b[a-z0-9_-]+-sys v[0-9.]+|\bring v[0-9.]+|\bcc v[0-9.]+|\baws-lc-[a-z]+ v[0-9.]+' \
    | sort -u)

if [ -z "$found" ]; then
    echo "  (none)"
else
    while read -r line; do
        name=${line%% *}
        version=${line##* }
        if [ -n "${EXPECTED[$name]:-}" ]; then
            printf '  \033[32mexpected\033[0m  %-14s %-10s %s\n' "$name" "$version" "${EXPECTED[$name]}"
        elif [ -n "${PURE_RUST_DESPITE_NAME[$name]:-}" ]; then
            printf '  \033[36mno C\033[0m      %-14s %-10s %s\n' "$name" "$version" "${PURE_RUST_DESPITE_NAME[$name]}"
        else
            printf '  \033[31mUNEXPECTED\033[0m %-14s %-10s no stated reason to link this\n' "$name" "$version"
            unexpected=$((unexpected + 1))
        fi
    done <<< "$found"
fi

echo
echo "== C libraries this tree links directly =="
# These are linked by the two sys crates rather than pulled in by a crate,
# so they do not appear in cargo tree at all. Each is checked for a
# pkg-config entry, or a header when the project ships no .pc file.
# setup-local-sysroot.sh puts VMM_SYSROOT in .cargo/config.toml, where cargo
# reads it but a shell does not.
if [ -z "${VMM_SYSROOT:-}" ]; then
    repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
    [ -f "$repo/.cargo/config.toml" ] &&
        VMM_SYSROOT=$(sed -n 's/^VMM_SYSROOT *= *"\(.*\)"/\1/p' "$repo/.cargo/config.toml" | tail -1)
fi
if [ -n "${VMM_SYSROOT:-}" ] && [ -d "$VMM_SYSROOT/usr/lib64/pkgconfig" ]; then
    export PKG_CONFIG_PATH="$VMM_SYSROOT/usr/lib64/pkgconfig:${PKG_CONFIG_PATH:-}"
fi
declare -A DIRECT=(
    [libavcodec]="video/audio encode and decode — §7.1"
    [libavutil]="shared FFmpeg types"
    [libswscale]="BGRA <-> I420 colour conversion — §7.1"
    [libva]="VA-API capability probe — §7.1"
    [libva-drm]="VA-API on a DRM render node — §7.1"
    [x264]="software H.264 encode fallback — §7.1"
    [vorbisenc]="Vorbis encode, the pre-Opus fallback — §7.1"
    [ogg]="Vorbis container dependency"
)
for lib in libavcodec libavutil libswscale libva libva-drm x264 vorbisenc ogg; do
    if version=$(pkg-config --modversion "$lib" 2>/dev/null); then
        printf '  \033[32mlinked\033[0m    %-14s %-10s %s\n' "$lib" "$version" "${DIRECT[$lib]}"
    else
        printf '  \033[31mMISSING\033[0m   %-14s %-10s %s\n' "$lib" "-" "${DIRECT[$lib]}"
        unexpected=$((unexpected + 1))
    fi
done
# VP9, AV1 and Opus are reached through libavcodec rather than linked, so
# they add no rows above — but a libavcodec built without them silently
# removes those codecs from what this host can offer. Name them here.
echo
echo "  reached through libavcodec (not separately linked):"
if command -v ffmpeg >/dev/null 2>&1; then
    encoders=$(ffmpeg -nostdin -hide_banner -encoders 2>/dev/null </dev/null)
    decoders=$(ffmpeg -nostdin -hide_banner -decoders 2>/dev/null </dev/null)
    # Herestrings, not `printf | grep -q`: this script sets pipefail, and
    # `grep -q` exits on the first match, SIGPIPEing the producer into the
    # pipeline's status.
    for spec in "libvpx-vp9:e:VP9 software encode — §7.1" \
                "libsvtav1:e:AV1 software encode — §7.1" \
                "libopus:e:Opus encode, the preferred audio codec — §7.1" \
                "libdav1d:d:AV1 decode in the client — §7.1"; do
        name=${spec%%:*}
        rest=${spec#*:}
        kind=${rest%%:*}
        reason=${rest#*:}
        haystack=$encoders
        [ "$kind" = d ] && haystack=$decoders
        if grep -q " $name " <<<"$haystack"; then
            printf '  \033[32mpresent\033[0m   %-14s %-10s %s\n' "$name" "(in libavcodec)" "$reason"
        else
            printf '  \033[33mabsent\033[0m    %-14s %-10s %s\n' "$name" "-" "$reason"
        fi
    done
else
    printf '        ffmpeg(1) is not installed, so this cannot be checked here\n'
fi
echo

# Ceph ships no pkg-config files, so probe for its headers instead.
for spec in "rados/librados.h:librados — rust_ceph_rbd engine, §5.4" \
            "rbd/librbd.h:librbd — RBD images and native snapshots, §10.2"; do
    header=${spec%%:*}
    reason=${spec#*:}
    if [ -e "/usr/include/$header" ] || [ -e "${VMM_SYSROOT:-}/usr/include/$header" ]; then
        printf '  \033[32mlinked\033[0m    %-14s %-10s %s\n' "${header%%/*}" "(header)" "$reason"
    else
        printf '  \033[31mMISSING\033[0m   %-14s %-10s %s\n' "${header%%/*}" "-" "$reason"
        unexpected=$((unexpected + 1))
    fi
done

echo
echo "== system libraries the binaries load =="
for bin in target/debug/custom-vmm target/debug/vmm-console-client; do
    [ -x "$bin" ] || continue
    echo "  ${bin##*/}:"
    ldd "$bin" | awk '{print $1}' | grep -vE '^(linux-vdso|/lib64/ld-linux)' \
        | sed 's/^/    /' | sort -u
done

echo
if [ "$unexpected" -gt 0 ]; then
    printf '\033[31m%d unexpected C dependency/dependencies\033[0m\n' "$unexpected"
    echo "Add it to EXPECTED here and to HOST-REQUIREMENTS.md §6, or remove it."
    exit 1
fi
echo "all C dependencies are accounted for"
