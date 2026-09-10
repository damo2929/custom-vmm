#!/usr/bin/env bash
# Check a host against HOST-REQUIREMENTS.md.
#
# Exit 0 if the hypervisor can boot a machine here, 1 if a hard requirement is
# missing. Optional items are reported but never fail the run.
#
#   ./scripts/preflight.sh [config.toml]
#
# With a config file, the paths and engines that machine actually names are
# checked too.

set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONFIG="${1:-}"
hard_fail=0
warnings=0

bold() { printf '\033[1m%s\033[0m\n' "$1"; }
ok()   { printf '  \033[32mok\033[0m    %s\n' "$1"; }
warn() { printf '  \033[33mwarn\033[0m  %s\n' "$1"; warnings=$((warnings + 1)); }
bad()  { printf '  \033[31mFAIL\033[0m  %s\n' "$1"; hard_fail=$((hard_fail + 1)); }
note() { printf '        %s\n' "$1"; }

bold "CPU and kernel"

if grep -qE '^flags.*\b(vmx|svm)\b' /proc/cpuinfo; then
    ok "hardware virtualisation ($(grep -oE '\b(vmx|svm)\b' /proc/cpuinfo | head -1))"
else
    bad "no vmx/svm: this CPU cannot run KVM"
fi

if grep -q '\bpdpe1gb\b' /proc/cpuinfo; then
    ok "pdpe1gb: 1 GiB hugepages supported"
else
    warn "no pdpe1gb: set hugepages_1gb = false (§1.3 wants 1 GiB pages)"
fi

printf '  kernel %s\n' "$(uname -r)"

bold "KVM"

if [ -c /dev/kvm ]; then
    if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
        ok "/dev/kvm is readable and writable"
    else
        bad "/dev/kvm exists but is not accessible: usermod -aG kvm \$USER"
    fi
else
    bad "/dev/kvm is missing: is the kvm_intel/kvm_amd module loaded?"
fi

# KVM_CAP_SPLIT_IRQCHIP is capability 118 and has no fallback (§1.4).
if [ -r /dev/kvm ]; then
    if python3 - <<'PY' 2>/dev/null
import fcntl, os, sys
KVM_CHECK_EXTENSION = 0xAE03
KVM_CAP_SPLIT_IRQCHIP = 121
try:
    fd = os.open("/dev/kvm", os.O_RDWR)
    sys.exit(0 if fcntl.ioctl(fd, KVM_CHECK_EXTENSION, KVM_CAP_SPLIT_IRQCHIP) > 0 else 1)
except Exception:
    sys.exit(2)
PY
    then
        ok "KVM_CAP_SPLIT_IRQCHIP available (no PIC/PIT, §1.4)"
    else
        case $? in
            1) bad "KVM_CAP_SPLIT_IRQCHIP unavailable: §1.4 forbids the legacy PIC/PIT fallback" ;;
            *) warn "could not probe KVM_CAP_SPLIT_IRQCHIP (needs python3); the VMM checks it at boot" ;;
        esac
    fi
fi

bold "Memory"

hp_dir=/sys/kernel/mm/hugepages/hugepages-1048576kB
if [ -d "$hp_dir" ]; then
    total=$(cat "$hp_dir/nr_hugepages" 2>/dev/null || echo 0)
    free=$(cat "$hp_dir/free_hugepages" 2>/dev/null || echo 0)
    if [ "$total" -gt 0 ]; then
        ok "1 GiB hugepages: $free free of $total reserved"
    else
        warn "no 1 GiB hugepages reserved (nr_hugepages = 0)"
        note "echo 8 | sudo tee $hp_dir/nr_hugepages"
        note "or boot with: default_hugepagesz=1G hugepagesz=1G hugepages=8"
    fi
else
    warn "the kernel exposes no 1 GiB hugepage pool"
fi

mounts=$(mount)
if grep -q 'hugetlbfs.*pagesize=1G' <<<"$mounts"; then
    ok "a 1 GiB hugetlbfs is mounted"
elif grep -q hugetlbfs <<<"$mounts"; then
    warn "hugetlbfs is mounted, but not with pagesize=1G"
    note "sudo mount -t hugetlbfs -o pagesize=1G none /dev/hugepages1G"
else
    warn "no hugetlbfs mount (only needed by the rust_hugepage_file engine)"
fi

memlock=$(ulimit -l)
if [ "$memlock" = "unlimited" ]; then
    ok "memlock is unlimited"
else
    warn "memlock is ${memlock} KiB; guest RAM must fit inside it"
    note "LimitMEMLOCK=infinity in the unit file, or ulimit -l unlimited"
fi

bold "Storage"

for dir in /var/lib/vmm /var/lib; do
    [ -d "$dir" ] || continue
    fstype=$(stat -f -c %T "$dir" 2>/dev/null)
    case "$fstype" in
        btrfs|xfs) ok "$dir is $fstype: FICLONE reflink snapshots work (§10.2)" ;;
        *)         warn "$dir is $fstype: no reflink, so snapshots fall back to a full copy" ;;
    esac
    break
done

if [ -r /proc/sys/kernel/io_uring_disabled ]; then
    case "$(cat /proc/sys/kernel/io_uring_disabled)" in
        0) ok "io_uring is enabled" ;;
        *) warn "io_uring is restricted or disabled (§5.5 will need it)" ;;
    esac
else
    ok "io_uring is not gated by sysctl"
fi

bold "Devices"

[ -d /dev/bus/usb ] && ok "/dev/bus/usb present (USB/IP export, §9)" \
                    || warn "/dev/bus/usb missing: no USB devices can be exported"
[ -d /dev/vfio ]    && ok "/dev/vfio present (rust_nvme engine, §5.4)" \
                    || warn "/dev/vfio missing: the rust_nvme engine cannot bind a namespace"
[ -e /dev/dri/renderD128 ] && ok "/dev/dri/renderD128 present (VA-API encode, §7.1)" \
                           || warn "no render node: VA-API H.264 encode unavailable"
ls /dev/nvidia* >/dev/null 2>&1 && ok "NVIDIA nodes present (NVENC encode, §7.1)" \
                                || note "no NVIDIA nodes (only matters if hardware_accelerator = \"nvenc\")"

bold "Firmware"

found_ovmf=""
for p in /usr/share/edk2/ovmf/OVMF_CODE.fd /usr/share/OVMF/OVMF_CODE.fd \
         /usr/share/edk2-ovmf/x64/OVMF_CODE.fd; do
    if [ -f "$p" ]; then
        found_ovmf="$p"
        break
    fi
done
if [ -n "$found_ovmf" ]; then
    size=$(stat -c %s "$found_ovmf")
    ok "OVMF at $found_ovmf ($((size / 1024)) KiB)"
    [ "$size" -le $((4 * 1024 * 1024)) ] || bad "OVMF exceeds the 4 MiB code region (§3.1)"
else
    bad "no OVMF_CODE.fd found: install edk2-ovmf"
fi

bold "Toolchain"

# rustup installs to ~/.cargo/bin, which a non-login shell may not have.
[ -d "$HOME/.cargo/bin" ] && PATH="$HOME/.cargo/bin:$PATH"

if command -v cargo >/dev/null; then
    ok "cargo $(cargo --version | awk '{print $2}')"
else
    bad "cargo not found: install Rust via rustup (or add ~/.cargo/bin to PATH)"
fi
command -v cargo-deny >/dev/null && ok "cargo-deny present (§1.1 gate)" \
                                 || note "cargo-deny absent; scripts/c-dependency-inventory.sh is the fallback"
command -v ffplay >/dev/null || command -v vlc >/dev/null \
    && ok "a player is available for the client's stored stream" \
    || note "no ffplay/vlc: the console stream can still be written to a file"

# The client's --display path (§5). No libwayland is needed — the protocol is
# spoken in Rust — so all this checks is that a session exists to talk to.
if [ -n "${WAYLAND_DISPLAY:-}" ] && [ -S "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/${WAYLAND_DISPLAY}" ]; then
    ok "Wayland session ${WAYLAND_DISPLAY}: the client can open a console window (--display)"
elif [ -n "${WAYLAND_DISPLAY:-}" ]; then
    warn "WAYLAND_DISPLAY=${WAYLAND_DISPLAY} but no such socket in ${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
    note "--display would fail with error 5012; --decode-to and --snapshot still work"
else
    note "no Wayland session: --display is unavailable here (error 5012)"
    note "--decode-to writes raw BGRA and --snapshot writes a PPM instead"
fi

# -- config-specific checks -------------------------------------------------

if [ -n "$CONFIG" ]; then
    bold "Configuration: $CONFIG"
    if [ ! -f "$CONFIG" ]; then
        bad "$CONFIG does not exist"
    else
        # Every path the machine names must exist, except images we can create.
        while IFS= read -r path; do
            [ -n "$path" ] || continue
            case "$path" in
                *.sock) [ -S "$path" ] && ok "socket $path" \
                        || note "vhost-user socket $path is absent (the back-end creates it)" ;;
                *)
                    if [ -f "$path" ]; then
                        if [ -s "$path" ]; then
                            ok "$path"
                        else
                            bad "$path is empty: provision it (truncate -s 32G $path)"
                        fi
                    else
                        case "$path" in
                            */msdm.bin|*/slic.bin)
                                warn "$path absent: that ACPI table is skipped, activation will not apply (§3.3)" ;;
                            *)
                                warn "$path does not exist yet" ;;
                        esac
                    fi
                    ;;
            esac
        done < <(grep -oE '"(/[^"]+)"' "$CONFIG" | tr -d '"' | grep -v '^/live$\|^/console$' | sort -u)

        # A volatile TPM engine is rejected at boot (§6.2).
        tpm_engine=$(awk '/^\[tpm.storage\]/{f=1} f && /engine *=/{print; exit}' "$CONFIG")
        if grep -q rust_hugepage_file <<<"$tpm_engine"; then
            bad "tpm.storage uses a volatile engine: boot aborts with Config(TpmVolatileEngine) 1020 (§6.2)"
        fi

        # Engines that are contract-only in this build.
        for engine in rust_nvme; do
            if grep -q "engine = \"$engine\"" "$CONFIG"; then
                warn "$engine is declared but not implemented in this build: DEVICE_INIT will fail with 4002"
            fi
        done

        # rust_ceph_rbd is implemented, but it needs a reachable cluster.
        if grep -q 'engine = "rust_ceph_rbd"' "$CONFIG"; then
            conf=$(grep -oE 'cluster_config *= *"[^"]+"' "$CONFIG" | head -1 | cut -d'"' -f2)
            if [ -n "$conf" ] && [ -r "$conf" ]; then
                ok "rust_ceph_rbd: $conf is readable"
            elif [ -r /etc/ceph/ceph.conf ]; then
                ok "rust_ceph_rbd: /etc/ceph/ceph.conf is readable"
            else
                warn "rust_ceph_rbd is declared but no readable ceph.conf was found: DEVICE_INIT will fail with 4002"
            fi
        fi
    fi
fi

bold "C libraries (HOST-REQUIREMENTS.md §6)"

# setup-local-sysroot.sh puts VMM_SYSROOT in .cargo/config.toml, where cargo
# reads it but a shell does not. Without this the build works while preflight
# reports every C library missing, which reads as a broken host rather than
# an unexported variable.
if [ -z "${VMM_SYSROOT:-}" ] && [ -f "$REPO/.cargo/config.toml" ]; then
    VMM_SYSROOT=$(sed -n 's/^VMM_SYSROOT *= *"\(.*\)"/\1/p' "$REPO/.cargo/config.toml" | tail -1)
fi

if [ -n "${VMM_SYSROOT:-}" ] && [ -d "$VMM_SYSROOT/usr/lib64/pkgconfig" ]; then
    note "using the local sysroot at $VMM_SYSROOT"
    export PKG_CONFIG_PATH="$VMM_SYSROOT/usr/lib64/pkgconfig:${PKG_CONFIG_PATH:-}"
fi

if command -v pkg-config >/dev/null; then
    for spec in "libavcodec:H.264 decode (client) and hardware encode" \
                "libavutil:shared FFmpeg types" \
                "libswscale:BGRA <-> I420 conversion (§7.1)" \
                "libva:VA-API capability probe (§7.1)" \
                "libva-drm:VA-API on a DRM render node (§7.1)" \
                "x264:software H.264 encode fallback (§7.1)" \
                "vorbisenc:Vorbis encode (§7.1)" \
                "ogg:Vorbis container dependency"; do
        lib=${spec%%:*}
        why=${spec#*:}
        if version=$(pkg-config --modversion "$lib" 2>/dev/null); then
            ok "$lib $version — $why"
        else
            bad "$lib is missing — $why"
        fi
    done
else
    bad "pkg-config is not installed: the codec libraries cannot be located"
fi

# Ceph ships no pkg-config files, so probe for its headers.
for spec in "rados/librados.h:librados — rust_ceph_rbd (§5.4)" \
            "rbd/librbd.h:librbd — RBD snapshots (§10.2)"; do
    header=${spec%%:*}
    why=${spec#*:}
    if [ -e "/usr/include/$header" ] || [ -e "${VMM_SYSROOT:-}/usr/include/$header" ]; then
        ok "$header — $why"
    else
        bad "$header is missing — $why"
    fi
done

# bindgen needs libclang, but not the clang driver.
# One `ls` over both globs would fail whenever either has no match, so the
# directories are tried separately.
libclang=""
for dir in /usr/lib64 /usr/lib; do
    for candidate in "$dir"/libclang.so*; do
        [ -e "$candidate" ] && libclang="$candidate" && break 2
    done
done
if [ -n "$libclang" ]; then
    resource=$(ls -d /usr/lib/clang/*/include /usr/lib64/clang/*/include 2>/dev/null | tail -1)
    if [ -n "$resource" ]; then
        ok "$(basename "$libclang") with builtin headers at $resource (bindgen)"
    else
        bad "libclang is present but its builtin headers are not: install clang-resource-filesystem"
    fi
else
    bad "libclang is missing: bindgen cannot run (install clang-libs)"
fi

bold "Codec support in libavcodec (§7.1)"

# VP9, AV1 and Opus are reached through libavcodec rather than linked
# directly, so pkg-config finding libavcodec says nothing about whether they
# are built into it. A distribution that omits one fails at encoder init,
# far from the cause, so name it here instead.
if command -v ffmpeg >/dev/null 2>&1; then
    # -nostdin because ffmpeg reads stdin by default. The greps below use
    # herestrings rather than `printf | grep -q`: this script sets pipefail,
    # and `grep -q` exits on the first match, SIGPIPEing the producer (141)
    # into the pipeline's status — which made the result a coin flip.
    encoders=$(ffmpeg -nostdin -hide_banner -encoders 2>/dev/null </dev/null)
    decoders=$(ffmpeg -nostdin -hide_banner -decoders 2>/dev/null </dev/null)
    for entry in "libvpx-vp9:VP9 software encode" \
                 "libsvtav1:AV1 software encode" \
                 "libopus:Opus encode" \
                 "libvorbis:Vorbis encode (the pre-Opus fallback)"; do
        name=${entry%%:*}
        what=${entry#*:}
        if grep -q " $name " <<<"$encoders"; then
            ok "$name — $what"
        else
            warn "libavcodec has no $name: $what is unavailable, so that codec will not be offered"
        fi
    done
    if grep -q " libdav1d " <<<"$decoders"; then
        ok "libdav1d — AV1 decode (the fastest AV1 decoder, and why AV1 scores well)"
    else
        warn "libavcodec has no libdav1d: AV1 decode falls back to a slower decoder"
    fi
else
    note "ffmpeg(1) is not installed, so the built-in codec list cannot be checked here"
    note "the boot log reports what libavcodec actually offers"
fi

bold "Hardware video encode (§7.1)"

if [ -e /dev/dri/renderD128 ]; then
    if [ -r /dev/dri/renderD128 ] && [ -w /dev/dri/renderD128 ]; then
        ok "/dev/dri/renderD128 is accessible"
        # On Fedora the usual reason hardware encode is unavailable is that
        # stock Mesa is built without H.264/HEVC for patent reasons, which
        # looks identical to a GPU that cannot encode. Say so, because the
        # fix is one package rather than different hardware.
        if [ -n "$(ls /usr/lib64/dri-freeworld/*_drv_video.so 2>/dev/null)" ] \
           || [ -n "${LIBVA_DRIVERS_PATH:-}" ]; then
            ok "a freeworld VA driver is present: H.264 hardware encode should be available"
        elif [ -d /usr/lib64/dri-freeworld ]; then
            note "stock Mesa is installed: it is built without H.264/HEVC, so H.264 has no"
            note "hardware entrypoint here. This is not a problem by itself — negotiation"
            note "will prefer AV1 or VP9 hardware if the client can decode them."
            note "sudo dnf install mesa-va-drivers-freeworld   # RPM Fusion, for H.264-only clients"
        fi
        note "the VA-API probe decides at boot; the boot log names the backend and the reason"
    else
        warn "/dev/dri/renderD128 exists but is not read-write for this user: add it to the 'render' group"
        note "every codec will encode in software"
    fi
else
    warn "no DRM render node: every codec will encode in software"
    note "H.264 uses libx264 (SIMD up to AVX-512, threaded) and negotiation will prefer it"
fi

# -- verdict ----------------------------------------------------------------

echo
if [ "$hard_fail" -gt 0 ]; then
    printf '\033[31m%d hard requirement(s) missing\033[0m'"$([ $warnings -gt 0 ] && echo ", $warnings warning(s)")"'\n' "$hard_fail"
    echo "See HOST-REQUIREMENTS.md."
    exit 1
fi
if [ "$warnings" -gt 0 ]; then
    printf '\033[33mready, with %d warning(s)\033[0m\n' "$warnings"
else
    printf '\033[32mready\033[0m\n'
fi
echo "See HOST-REQUIREMENTS.md for what each item is for."
