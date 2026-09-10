# What is needed outside Rust

Everything the suite needs from the world beyond `cargo build`: kernel
features, device nodes, firmware blobs, filesystem properties, privileges,
and the small set of C libraries that §1.1 forbids linking — which is where
the hard problems are.

Run `./scripts/preflight.sh` to check a host against all of it.

Numbers below were verified on the development host (Fedora, kernel 7.1.13);
the minimum kernel versions are the upstream ones where each interface
landed.

---

## 1. Kernel interfaces

| Interface | Used for | Minimum kernel | Required? |
|---|---|---|---|
| `/dev/kvm` | the whole hypervisor | any | **hard** |
| `KVM_CAP_USER_MEMORY` | guest RAM slots (§1.4) | 2.6.x | **hard** |
| `KVM_CAP_SPLIT_IRQCHIP` | in-kernel LAPIC, userspace IOAPIC, **no PIC/PIT** (§1.4) | 4.4 | **hard** |
| `KVM_CAP_IRQ_ROUTING` | MSI-X GSI routes (§2.2) | 2.6.x | **hard** |
| `KVM_CAP_IRQFD` | MSI-X injection (§2.2) | 2.6.x | **hard** |
| `KVM_CAP_IOEVENTFD` | virtqueue doorbells without a VM exit (§2.2) | 2.6.x | **hard** |
| `hugetlbfs`, 1 GiB pages | guest RAM (§1.3) | 2.6.x + CPU `pdpe1gb` | **hard** unless `hugepages_1gb = false` |
| `fallocate(FALLOC_FL_PUNCH_HOLE)` | SCSI UNMAP (§5.3) | 2.6.38 | for discard |
| `FICLONE` reflink | makes a §10.2 backup *instant* rather than a full copy | XFS 4.16 / Btrfs 3.x | **no** — optimisation only |
| `io_uring` + `IORING_SETUP_SQPOLL` | the §5.5 datapath | 5.13 | **not yet used** |
| `usbdevfs` (`/dev/bus/usb`) | USB/IP URB relay (§9.2) | any | for USB export |
| `AF_XDP` | `rust_af_xdp` net datapath (§11) | 4.18 | **not yet used** |
| `VFIO` (`/dev/vfio`) | `rust_nvme` engine (§5.4) | 3.6 | **not yet used** |
| `getrandom(2)` | WebSocket masking keys (RFC 6455 §5.3) | 3.17 | **hard** |
| `ioprio_set`, `setpriority` | backup worker priority (§10.1) | any | for backups |

`KVM_CAP_SPLIT_IRQCHIP` is the one with no fallback. §1.4 forbids a legacy
PIC/PIT, so if the host cannot enable it the VMM aborts with
`Kvm(MissingCapability)` 2004 rather than falling back to a full in-kernel
irqchip.

### CPU

* x86-64 with `vmx` (Intel VT-x) or `svm` (AMD-V) — nested virtualisation is
  fine for development.
* `pdpe1gb` for 1 GiB hugepages. Without it, set `hugepages_1gb = false` and
  accept 4 KiB or 2 MiB backing.

---

## 2. Host configuration

### 1 GiB hugepages

Not reserved by default anywhere. For the §11 reference machine (8 GiB):

```sh
# Runtime, best effort — fails once memory is fragmented:
echo 8 | sudo tee /sys/kernel/mm/hugepages/hugepages-1048576kB/nr_hugepages

# Reliable: reserve at boot.
#   default_hugepagesz=1G hugepagesz=1G hugepages=8
sudo grubby --update-kernel=ALL --args="default_hugepagesz=1G hugepagesz=1G hugepages=8"
```

If the pool cannot cover the machine, MEM_ALLOC refuses with error 2006 and
names the `nr_hugepages` value needed. It is checked up front deliberately:
`mmap(MAP_HUGETLB)` succeeds against an empty pool and reserves lazily, so
without the check the failure arrives as a **SIGBUS on first touch** — a core
dump during firmware load, with nothing pointing at hugepages as the cause.
Set `memory.hugepages_1gb = false` to run on a host without a pool.

The `rust_hugepage_file` engine additionally wants a hugetlbfs **mounted with
a 1 GiB page size** if its backing file is to use 1 GiB pages. A distribution
default `/dev/hugepages` is usually `pagesize=2M`:

```sh
sudo mount -t hugetlbfs -o pagesize=1G none /dev/hugepages1G
```

### UEFI firmware

The machine boots firmware built from `OvmfPkg/CloudHv` (Revision D.9). No
distribution packages it, so build it once:

```sh
scripts/build-cloudhv-firmware.sh
```

**Root is not required.** The script needs `git`, `gcc`, `make` and
`python3`, plus `nasm` and `iasl` — and where those two are missing it
downloads their RPMs as an ordinary user and unpacks them into its own work
directory, the same technique `setup-local-sysroot.sh` uses for the C
libraries. With root you can install them the usual way instead:

```sh
sudo dnf install -y nasm acpica-tools
```

The alternative Q35 path (Revision D.2 option A) runs the distribution's own
firmware and needs no build at all — `dnf install edk2-ovmf`, then
`/usr/share/edk2/ovmf/OVMF_CODE.fd`. See [firmware/README.md](firmware/README.md).

### CPU baseline

The tree is built with `-C target-cpu=x86-64-v3` (set in the checked-in
`.cargo/config.toml`), so the host CPU must implement **x86-64-v3**: AVX2,
BMI1, BMI2, FMA, MOVBE, F16C and LZCNT on top of the v2 baseline. Every
server part since Haswell (2013) and Zen 1 (2017) qualifies; check with

```sh
/lib64/ld-linux-x86-64.so.2 --help | grep x86-64-v3
```

which prints `(supported, searched)` on a capable host. A binary built here
raises `SIGILL` on an older CPU rather than running slowly — that is
deliberate. To build for an older host, override the flag:

```sh
RUSTFLAGS="-C target-cpu=x86-64-v2" cargo build --release
```

### Filesystem

Any filesystem works. A §10.2 backup is a **full byte-for-byte copy** of the
drive image — that is the proper backup, and it is what the engine produces
by default and on every filesystem.

Reflink is purely an optimisation on top of that. On **XFS or Btrfs** the
engine issues `FICLONE`, which produces the same complete, independent
snapshot in constant time instead of copying every block. Where `FICLONE` is
unavailable or fails, the engine falls back to the full copy, logs that it
did so, and records `FullCopy` rather than `Reflink` in the manifest. Both
methods yield a backup of equal integrity; only the time and the transient
disk usage differ.

Reflink is therefore never a prerequisite for taking a backup, and no
configuration may refuse to back up because a filesystem lacks it.

### Limits

* `memlock` — must accommodate the whole guest RAM plus BAR mappings.
  `LimitMEMLOCK=infinity` in a unit file, or `ulimit -l unlimited`.

---

## 3. Privileges

| Need | Why | How |
|---|---|---|
| read/write `/dev/kvm` | everything | `usermod -aG kvm $USER` (many distros ship it `0666`) |
| write `/dev/bus/usb/BBB/DDD` | USB/IP export detaches the kernel driver (§9.2) | a udev rule, or run the client as root |
| `CAP_NET_ADMIN`, `CAP_BPF` | AF_XDP socket and XDP program load | capabilities on the binary, or root |
| `CAP_SYS_NICE` | not needed — `nice(19)` lowers priority | — |
| VFIO group access | rebinding an NVMe namespace away from the kernel driver | `/dev/vfio/<group>` ownership |

The VMM does **not** need root for the KVM, memory, ACPI, firmware or
control-plane paths. Root creeps in only at the device-passthrough edges:
USB, AF_XDP and VFIO.

---

## 4. Files the configuration points at

The §11 reference machine names these; none are produced by this repo.

| Path | What it is | Where it comes from |
|---|---|---|
| `firmware.code_path` | `OVMF_CODE.fd`, 4 MiB, mapped read-only at `0xFFC0_0000` | `edk2-ovmf` package. Fedora: `/usr/share/edk2/ovmf/OVMF_CODE.fd`; Debian: `/usr/share/OVMF/OVMF_CODE.fd` |
| `firmware.vars_path` | `OVMF_VARS.fd` variable-store template | same package |
| `firmware.storage.file_path` | the live EFI NVRAM image | copy the `VARS` template once |
| `tpm.storage.file_path` | TPM NV state | created empty; **must be on a persistent engine** (§6.2) |
| `storage.drives[].file_path` | raw disk images | **provision them yourself** — §11 has no size field, and the VMM refuses a zero-length image: `truncate -s 32G boot_os.raw` |
| `acpi.msdm_path` | OEM Windows licence key table | the OEM/vendor firmware. Optional: absent means activation will not apply, and boot continues with a warning (§3.3) |
| `acpi.slic_path` | OA 2.1 licence table | same |
| `storage.drives[].socket_path` | vhost-user back-end sockets | created by the back-end process, which does not exist yet (§6) |
| Ceph `cluster_config` | `/etc/ceph/ceph.conf` + a keyring | a running Ceph cluster |

### Guest-side

* **A guest agent on virtio-serial** for the §10.1 quiesce
  (`guest-fsfreeze-freeze`). Without one the backup proceeds
  crash-consistent after the 10s timeout and logs warning 8005 — that is
  specified behaviour, not a failure.
* Windows 11 guests want the **virtio driver set** (viostor/vioscsi,
  NetKVM, viogpu, viorng, vioserial) — the Fedora `virtio-win` ISO.

---

## 5. Client-side, outside Rust

* **A Wayland compositor.** The console client opens a window by default —
  viewing and driving the VM is what it is for — so this is a requirement,
  not an option. It maps an `xdg-shell` toplevel, shares frames through
  `wl_shm`, and takes input from `wl_seat`, so any compositor implementing
  `wl_compositor`, `xdg_wm_base`, `wl_shm` and `wl_seat` will do — which is
  all of them. Verified against GNOME/Mutter. It needs `WAYLAND_DISPLAY` and
  `XDG_RUNTIME_DIR` set, as a session normally does; without them the client
  fails with error 5012 naming both, rather than opening nothing and looking
  hung.

  For headless use — a recording box, CI — pass `--no-display`, or
  `--video-out`, which implies it.

  There is **no X11 path**. Running under XWayland is not needed and not
  supported: the client speaks the Wayland protocol directly. On an X11-only
  desktop, use `--no-display` with `--decode-to` or `--snapshot` and view the
  result — with no input, since there is no window to capture it.

  No libwayland is required at build or run time — the protocol is spoken in
  Rust over the compositor socket.

* **A reachable §8 control endpoint**, if the console is to be interactive.
  The media stream is one-way, so keyboard and pointer go back over the
  control channel (`--control-addr`, default `[::1]:8080`). Without it the
  window still opens, watch-only.

* **A media player**, only if you store the stream rather than watching it.
  `--video-out` writes the elementary stream, which `ffplay` or VLC will take
  when the negotiated codec is H.264. `--snapshot` writes a PPM, which needs
  no player at all.

* **A terminal** for input capture — the client needs a real TTY to enter
  raw mode. Piped stdin degrades to watch-only, and says so.

---

## 6. C libraries

**§1.1's no-C rule has been lifted.** It blocked six subsystems, and only two
of them had a pure-Rust path. What replaces it is an allow-list in
`deny.toml`: C is permitted, but only the dependencies we chose on purpose —
an accidental `-sys` crate arriving transitively still fails.

`./scripts/c-dependency-inventory.sh` prints what is actually linked and
fails on anything unaccounted for.

### Linked today

| § | Library | Reached via | Buys |
|---|---|---|---|
| 8.2, 7.4, 9.2 | BoringSSL-derived crypto | `ring` (via `rustls`) | TLS 1.3 on every listener and both clients |
| 10.3 | `libzstd` | `zstd` → `zstd-sys` | the `.vmbk` stream, with frame checksums |
| 7.1 | `libva`, `libva-drm` | `vmm-codec-sys` | the VA-API capability probe that decides hardware vs software encode |
| 7.1 | `libavcodec`, `libavutil` | `vmm-codec-sys` | hardware encode (`h264_vaapi`, `vp9_vaapi`, `av1_vaapi`), the VP9/AV1/Opus software encoders, and all decode in the client |
| 7.1 | `libswscale` | `vmm-codec-sys` | BGRA ⇄ I420, both capture and client render |
| 7.1 | `libx264` | `vmm-codec-sys` (C shim) | software H.264 encode, the fallback path |
| 7.1 | `libvorbisenc`, `libvorbis`, `libogg` | `vmm-codec-sys` | Vorbis encode, the fallback for clients without Opus |
| 5.4, 10.2 | `librados`, `librbd` | `vmm-rbd-sys` | the `rust_ceph_rbd` engine and its native snapshots |

VP9, AV1 and Opus are reached **through libavcodec** rather than linked
directly, so they add no entries to the table above — but libavcodec must
have been built with them. The encoders used are `libvpx-vp9`, `libsvtav1`
and `libopus`; decode uses `libdav1d` for AV1. `scripts/preflight.sh` checks
for each and names the missing one rather than failing at encoder init.

`ring` and `zstd-sys` are **statically** linked and need nothing installed on
the target. Everything below them is a shared library: the `-devel` packages
in §7 are needed to build, and the matching runtime `.so` to run.

#### How each is bound

`libva`, FFmpeg, Vorbis and Ceph are bound with `bindgen` at build time.
**libx264 is the exception** and goes through a small C shim in
`crates/vmm-codec-sys/shim/`. The reason is a bindgen limitation rather than
a preference: `x264.h` defines `x264_zone_t` before `x264_param_t`, and the
zone struct holds a `struct x264_param_t *` back-pointer. Reaching that
forward declaration first makes bindgen emit `x264_param_t` as an opaque
one-byte struct and never upgrade it when the real definition appears, so
every encoder parameter becomes unreachable. Reproduced on bindgen 0.70,
0.71 and 0.72; blocklisting the zone type suppresses emission but not
materialisation. The shim's ABI is ours — scalars and pointers only — which
is what makes it safe to bind by hand.

#### What the hardware path actually requires

VA-API encode needs more than "a GPU and a driver". The probe in
`vmm-codec-sys` opens the DRM render node and asks, for **each** of H.264,
VP9 and AV1, whether a profile carries an encode entrypoint — because that
is the only reliable answer, and because the three answers differ on the
same machine.

**On Fedora the usual reason H.264 declines is packaging, not hardware.**
Stock `mesa-dri-drivers` is built without H.264 and HEVC for patent reasons,
so the probe reports JPEG, VP9 and AV1 only — no H.264 entrypoint at all,
encode or decode — on a GPU that supports it perfectly well. Since Revision B
this is no longer fatal to hardware encode: AV1 is still offered with an
entrypoint, and negotiation simply selects it instead. RPM Fusion restores
H.264 for clients that can only decode that:

```sh
sudo dnf install mesa-va-drivers-freeworld
```

That installs into `/usr/lib64/dri-freeworld`, which libva searches before
the stock path, so nothing else needs configuring. To test without
installing it, extract the RPM and point `LIBVA_DRIVERS_PATH` at the
extracted directory.

Genuine hardware limits exist too — some recent AMD and Intel parts ship AV1
encode with no H.264 encode — which is why the probe asks rather than
assumes. The boot log reports the whole picture, then the choice and its
reason:

```
can encode: video ["h264/sw", "vp9/sw", "av1/hw"], audio ["opus", "vorbis"]
codec selection: av1 (hardware) + opus — av1 chosen for hardware encode
(score 55); next best h264 at 140
```

Where no hardware entrypoint exists for the negotiated codec, encode falls
back to software — `libx264` for H.264, still SIMD (up to AVX-512) and
multi-threaded. Only `CodecError::Unavailable` crosses over. A host that
*has* the encoder but fails to open it is a real fault and is raised,
because degrading silently would hide it behind a performance regression
nobody would attribute correctly.

### Still pure Rust by choice

Three things have good pure-Rust implementations, so there is no reason to
link C for them:

* **Wayland** — `wayland-client` + `wayland-protocols`, for the console
  client's `--display` window. The default backend speaks the wire protocol
  over the compositor socket in Rust, so **libwayland is not needed at build
  or run time**. `wayland-sys` appears in the dependency graph but links
  nothing: its build script emits no link directive, and `ldd` on the client
  shows no `libwayland-client`. `scripts/c-dependency-inventory.sh` lists it
  under "no C" for exactly that reason.
* **io_uring** — `io-uring` (tokio-rs). §5.5's SQPOLL datapath.
* **AF_XDP** — `xsk-rs` / `afxdp` over the raw syscalls. Loading an XDP
  *program* needs BPF bytecode, which is a build artifact, not a linked
  library.

---

## 7. Build-time

* **Rust stable ≥ 1.75** (`rust-toolchain.toml` pins the channel). Installed
  here via `rustup`, at `~/.cargo/bin`.
* **A C toolchain** — `gcc` and `pkg-config`. `ring` and `zstd-sys` compile C
  and assembly in their build scripts.
* **Development headers** for the shared libraries in §6. On Fedora:

  ```sh
  sudo dnf install ffmpeg-devel libva-devel libvorbis-devel \
                   librados-devel librbd-devel x264-devel
  ```

  On Debian/Ubuntu the equivalents are `libavcodec-dev libswscale-dev
  libva-dev libvorbis-dev librados-dev librbd-dev libx264-dev`.

* **libclang**, for `bindgen`. Fedora's `clang-libs` and
  `clang-resource-filesystem` are enough; the `clang` driver itself is not
  needed. `clang-libs` puts `libclang.so` in `/usr/lib64` but its builtin
  headers in `/usr/lib/clang/<major>`, so libclang cannot locate its own
  resource directory. `crates/vmm-sysdeps` finds it and passes
  `-resource-dir`; set `CLANG_RESOURCE_DIR` to override.

* **No root?** `scripts/setup-local-sysroot.sh` stages the same `-devel`
  packages into a scratch directory with `dnf download` and writes a
  `.cargo/config.toml` pointing `VMM_SYSROOT` at it. Headers and link stubs
  come from the sysroot, the runtime `.so` files from the host. It is a
  development workaround: a production build should install the packages.
  `ffmpeg-devel` comes from RPM Fusion on Fedora.

  Two details of that script are worth knowing, because both produced a
  sysroot that looked fine and was not. RPMs carry their installed modes, and
  the `filesystem` package ships `/usr/lib64` read-only; once it has been
  unpacked, every later package silently loses whatever it would have written
  there — which is how a sysroot came to be missing the unversioned `.so`
  symlinks for libva, libvorbis, libogg and x264 while the script reported
  success. The script now reopens those directories before every package.
  And because `pkg-config --modversion` answers from a `.pc` file alone, it
  cannot detect that, so the script finishes by **linking a real program**
  against all eight libraries. If that link fails the script fails, rather
  than handing you a sysroot that only breaks later at `cargo build`.

  `preflight.sh` and `c-dependency-inventory.sh` read `VMM_SYSROOT` from
  `.cargo/config.toml` when it is not exported, so they agree with what cargo
  will actually use.

* **`cargo-deny`** for the dependency allow-list: `cargo install cargo-deny`.
  `./scripts/c-dependency-inventory.sh` is a dependency-free equivalent that
  needs only `cargo tree`, `ldd` and a shell.
* **Network access to crates.io** for the first build; `cargo vendor`
  afterwards for an air-gapped one.
