#!/bin/bash
#
# Build CLOUDHV.fd — the UEFI firmware this hypervisor boots.
#
# Fedora's edk2-ovmf package ships OVMF_CODE.fd, MICROVM.fd and several
# confidential-computing variants, but *not* CLOUDHV.fd. Nor does any other
# distribution, so this firmware has to be built. That is the one cost of
# Revision D.2 option B, and this script is it.
#
# What comes out is an **ELF**, not a flash image: OvmfPkg/CloudHv builds
# with OvmfPkg/XenResetVector, which carries an `XEN_ELFNOTE_PHYS32_ENTRY`
# note. So it is loaded by the same PVH loader that loads a Linux kernel
# (crates/libvmm-core/src/pvh.rs) rather than being mapped at the reset
# vector — see docs/spec-revision-D-boot-and-platform.md, D.2 and D.9.
#
# Usage:
#     scripts/build-cloudhv-firmware.sh [output-path]
#
# Default output is firmware/CLOUDHV.fd under the repository root.
#
# Root is not required. If nasm and iasl are missing they are downloaded as
# RPMs and unpacked into the work directory, the same trick
# setup-local-sysroot.sh uses for the C libraries.
#
# Environment:
#     CLOUDHV_WORKDIR   where edk2 is cloned and built (default: a directory
#                       beside this repository, so a rebuild is incremental)
#     CLOUDHV_EDK2_REF  git ref to build (default: the pinned tag below)
#
set -uo pipefail

# The edk2 revision this was last built and booted against. Pinning it means
# a rebuild six months from now produces the firmware this tree was tested
# with, rather than whatever master happens to be. Move it deliberately.
EDK2_REPO="https://github.com/tianocore/edk2.git"
EDK2_REF="${CLOUDHV_EDK2_REF:-master}"

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="${1:-$REPO/firmware/CLOUDHV.fd}"
WORK="${CLOUDHV_WORKDIR:-$REPO/../.cloudhv-build}"
EDK2="$WORK/edk2"
TOOLS="$WORK/tools"

need() {
    command -v "$1" >/dev/null 2>&1 || { echo "missing required tool: $1" >&2; exit 1; }
}
need git; need gcc; need make; need python3

# --- nasm and iasl, without root -------------------------------------------
#
# edk2 needs both: nasm for the reset vector and the SEC entry, iasl for the
# DSDT. Where the host has them we use them; where it does not, dnf can still
# *download* an RPM as an ordinary user and rpm2cpio can unpack it into a
# directory we own.
stage_build_tools() {
    if command -v nasm >/dev/null 2>&1 && command -v iasl >/dev/null 2>&1; then
        echo "==> nasm and iasl found on PATH"
        return 0
    fi
    echo "==> nasm or iasl missing; staging them into $TOOLS"
    need dnf; need rpm2cpio; need cpio
    mkdir -p "$TOOLS/rpms" "$TOOLS/root"
    if ! dnf download --resolve --destdir "$TOOLS/rpms" nasm acpica-tools >/dev/null 2>&1; then
        echo "could not download nasm/acpica-tools; install them with:" >&2
        echo "    sudo dnf install -y nasm acpica-tools" >&2
        exit 1
    fi
    # cpio's status is ignored deliberately and only here: an RPM whose
    # payload includes a directory we already have unpacked exits non-zero
    # having still written every file. The gate is the tool check below, not
    # this exit code.
    local rpm
    for rpm in "$TOOLS"/rpms/*.rpm; do
        ( cd "$TOOLS/root" && rpm2cpio "$rpm" | cpio -idmu --quiet ) 2>/dev/null
    done
    export PATH="$TOOLS/root/usr/bin:$PATH"
    need nasm; need iasl
    echo "    nasm $(nasm -v | head -1)"
    echo "    iasl $(iasl -v 2>&1 | sed -n '3p')"
}

# --- edk2 ------------------------------------------------------------------
fetch_edk2() {
    if [ -d "$EDK2/.git" ]; then
        echo "==> reusing $EDK2"
    else
        echo "==> cloning edk2 into $EDK2 — ~2.8 GB with submodules, several minutes"
        mkdir -p "$WORK"
        git clone --recurse-submodules --shallow-submodules --depth 1 \
            --branch "$EDK2_REF" "$EDK2_REPO" "$EDK2" || exit 1
    fi
    ( cd "$EDK2" && git submodule update --init --recursive --depth 1 ) || exit 1
}

# Add VirtioGpuDxe to the CloudHv build.
#
# Upstream's CloudHv target has no graphics driver at all: Cloud Hypervisor
# is a serial-console machine, so `CloudHvX64.dsc` ships VirtioBlk, VirtioScsi
# and VirtioRng and stops there. This hypervisor has a virtio-gpu and a
# console client looking at it, so the firmware needs to be able to draw.
#
# The driver is stock — `OvmfPkg/VirtioGpuDxe`, exactly as `OvmfPkgX64.dsc`
# includes it. Only the two lines that build it into this target are ours.
#
# Both edits are idempotent, so a rebuild against an existing clone is safe.
# The CloudHv target has no display driver at all: Cloud Hypervisor's own
# consoles are serial. One is added here — QemuVideoDxe, which binds the
# Bochs VBE display at 1234:1111 and reports a real linear framebuffer.
#
# VirtioGpuDxe is deliberately *not* added, even though the machine has a
# virtio-gpu. Its GOP reports PixelBltOnly and never sets FrameBufferBase,
# so it stops working the moment a guest calls ExitBootServices; and with
# both drivers present the firmware publishes two graphics protocols, of
# which an operating system picks one. It picked the wrong one. The
# firmware's console belongs on the surface that survives the handover.
#
# virtio-gpu remains on the bus for guests that drive it themselves.
# See crates/libvmm-core/src/display.rs and Revision D.10.
add_display_drivers() {
    "$PYTHON_COMMAND" - "$EDK2" <<'PYEOF'
import subprocess, sys, pathlib

edk2 = pathlib.Path(sys.argv[1])
drivers = [
    ("QemuVideoDxe", "OvmfPkg/QemuVideoDxe/QemuVideoDxe.inf"),
]
for name, inf in drivers:
    if not (edk2 / inf).is_file():
        sys.exit(f"{inf} is missing; this edk2 checkout is not what we expect")

# edk2's DSC and FDF files use CRLF. A shell `sed` with a `$` anchor silently
# matches nothing against them, and the only symptom is a firmware with no
# graphics driver — so this is done here, where the line endings are explicit.
targets = [
    ("OvmfPkg/CloudHv/CloudHvX64.dsc", "  OvmfPkg/VirtioRngDxe/VirtioRng.inf", "  {inf}"),
    ("OvmfPkg/CloudHv/CloudHvX64.fdf", "INF  OvmfPkg/VirtioRngDxe/VirtioRng.inf", "INF  {inf}"),
]

# Start from the checked-out files every time. Without this the edits are
# only additive, and a driver removed from the list above would stay in a
# working tree that had already been patched.
subprocess.run(["git", "-C", str(edk2), "checkout", "--"] + [rel for rel, _, _ in targets],
               check=True)

for rel, anchor, shape in targets:
    path = edk2 / rel
    text = path.read_text()
    wanted = [(n, shape.format(inf=i)) for n, i in drivers if n not in text]
    if not wanted:
        continue
    lines = text.split("\n")
    out = []
    added = False
    for line in lines:
        out.append(line)
        # Compare without the carriage return, then re-add it so the file
        # keeps the line endings it came with.
        if not added and line.rstrip("\r") == anchor:
            carriage = "\r" if line.endswith("\r") else ""
            for _, addition in wanted:
                out.append(addition + carriage)
            added = True
    if not added:
        sys.exit(f"{rel}: could not find the line to add the display drivers after")
    path.write_text("\n".join(out))

# Verify rather than trust.
for rel, _, _ in targets:
    text = (edk2 / rel).read_text()
    for name, _ in drivers:
        if name not in text:
            sys.exit(f"{rel}: {name} was not added")
print("==> QemuVideoDxe added to the CloudHv target")
PYEOF
}

build_firmware() {
    # edksetup.sh reads WORKSPACE before assigning it and is not `set -u`
    # clean, so -u is off for exactly this section. Everything it needs is
    # exported first so nothing is left to its defaults.
    set +u
    export PYTHON_COMMAND=/usr/bin/python3
    export WORKSPACE="$EDK2"
    cd "$EDK2" || exit 1

    echo "==> building BaseTools"
    if ! make -C BaseTools -j"$(nproc)" >"$WORK/basetools.log" 2>&1; then
        echo "BaseTools failed; last 20 lines of $WORK/basetools.log:" >&2
        tail -20 "$WORK/basetools.log" >&2
        exit 1
    fi

    . ./edksetup.sh BaseTools >/dev/null 2>&1

    # The toolchain tag is `GCC` on current edk2; it was `GCC5` until the
    # tags were consolidated. Pick whichever this tree defines rather than
    # guessing, because the error for a wrong tag ("No toolchain available")
    # does not say that a different one would have worked.
    local tag=GCC
    if ! grep -q '^\*_GCC_' BaseTools/Conf/tools_def.template; then
        tag=GCC5
    fi
    add_display_drivers || exit 1

    echo "==> building OvmfPkg/CloudHv/CloudHvX64.dsc with -t $tag"

    # DEBUG_ON_SERIAL_PORT sends the firmware's DEBUG() output to the 16550
    # at 0x3F8, which this hypervisor emulates and logs. Without it the
    # output goes to edk2's 0x402 debug port only, and a RELEASE build emits
    # almost nothing at all — which is the difference between diagnosing a
    # firmware hang and guessing at it.
    build -a X64 -t "$tag" -p OvmfPkg/CloudHv/CloudHvX64.dsc -b RELEASE \
          -D DEBUG_ON_SERIAL_PORT=TRUE -n "$(nproc)"
    local status=$?
    set -u
    return $status
}

stage_build_tools
fetch_edk2
if ! build_firmware; then
    echo "edk2 build failed" >&2
    exit 1
fi

BUILT="$EDK2/Build/CloudHvX64/RELEASE_GCC/FV/CLOUDHV.fd"
if [ ! -f "$BUILT" ]; then
    # The output directory carries the toolchain tag, so find it rather than
    # assume it.
    BUILT="$(find "$EDK2/Build" -name CLOUDHV.fd -printf '%T@ %p\n' 2>/dev/null \
             | sort -rn | head -1 | cut -d' ' -f2-)"
fi
if [ -z "${BUILT:-}" ] || [ ! -f "$BUILT" ]; then
    echo "build reported success but produced no CLOUDHV.fd" >&2
    exit 1
fi

# Verify what we are about to ship rather than trusting the build. The whole
# reason this image works here is that it is an ELF carrying a PVH entry
# point; a flash-image build would be silently useless to the PVH loader.
if [ "$(head -c 4 "$BUILT" | od -An -tx1 | tr -d ' ')" != "7f454c46" ]; then
    echo "$BUILT is not an ELF — the PVH loader cannot boot it" >&2
    exit 1
fi

mkdir -p "$(dirname "$OUT")"
cp "$BUILT" "$OUT" || exit 1
echo
echo "==> wrote $OUT ($(stat -c%s "$OUT") bytes)"
echo "    verify the PVH entry point with:"
echo "        cargo run -p libvmm-core --example pvh_probe -- $OUT"
echo "    boot it with:"
echo "        cargo run --release -p custom-vmm --example cloudhv_boot -- $OUT"
