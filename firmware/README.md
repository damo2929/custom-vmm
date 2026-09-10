# firmware/

Build artifacts. Nothing here is tracked by git — see `.gitignore`.

## CLOUDHV.fd

The UEFI firmware this hypervisor boots (Revision D.2 option B). No
distribution packages it, so it has to be built:

```sh
scripts/build-cloudhv-firmware.sh
```

That script needs no root: where `nasm` and `iasl` are missing it downloads
the RPMs as an ordinary user and unpacks them into its own work directory.
It clones edk2 into `../.cloudhv-build` by default (override with
`CLOUDHV_WORKDIR`), builds `OvmfPkg/CloudHv/CloudHvX64.dsc`, verifies the
result is an ELF, and copies it here.

Budget for the first run: the clone is **~2.8 GB** with submodules and takes
several minutes; the build itself is **~70 seconds**. A rebuild against an
existing clone is just the 70 seconds. The work directory is outside the
repository so a `git clean` does not throw it away.

Check what you got and boot it:

```sh
cargo run -p libvmm-core --example pvh_probe -- firmware/CLOUDHV.fd
# firmware/CLOUDHV.fd: ELF, PVH entry 0x4fffd0, loads 0x100000..0x500000

cargo run --release -p custom-vmm --example cloudhv_boot -- firmware/CLOUDHV.fd
```

It is an **ELF**, not a flash image, because `OvmfPkg/CloudHv` builds with
`OvmfPkg/XenResetVector` and carries an `XEN_ELFNOTE_PHYS32_ENTRY` note. So
it loads through the same PVH loader as a Linux kernel
(`crates/libvmm-core/src/pvh.rs`) rather than being mapped at the reset
vector. `scripts/build-cloudhv-firmware.sh` checks the ELF magic before
copying, because a flash-image build would be silently useless here.

## Stock OVMF

The Q35 path (Revision D.2 option A) needs no build — it runs Fedora's own
package unmodified:

```sh
cargo run --release -p custom-vmm --example ovmf_boot -- \
    /usr/share/edk2/ovmf/OVMF_CODE.fd /usr/share/edk2/ovmf/OVMF_VARS.fd
```
