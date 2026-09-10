# Error codes — Appendix A

Every fallible operation in the suite returns `Result<T, VmmError>`, and
every `VmmError` carries a **stable numeric code**. The code is what appears
in the boot log, in WSS error frames and in RTSP error responses, and it is
the thing to search for when something fails. Codes never change meaning;
new conditions get new codes.

```rust
let code = err.code();      // 5011
let domain = err.domain();  // "Media"
```

The families follow the top-level domain:

| range | domain | raised by |
|---|---|---|
| 1xxx | Config | TOML load and validation, before anything is allocated |
| 2xxx | KVM | `/dev/kvm` ioctls, memory, vCPU state |
| 3xxx | Virtio | virtqueue and transport handshake |
| 4xxx | Storage | the four engines and the SCSI command set |
| 5xxx | Media | capture, encode, RTP, RTSP |
| 6xxx | Control | the WSS listener, auth and protocol v1 |
| 7xxx | USB/IP | the wire protocol and the relay |
| 8xxx | Backup | snapshot, quiesce and the `.vmbk` stream |

Two codes sit outside their obvious family on purpose. Storage **4012**
(engine cannot snapshot) surfaces to a backup caller as **8001**, because the
backup is what failed from the operator's point of view. Backup **8011**
mirrors control **6423** for the same reason: the CLI and the WSS action
report the same condition, and each reports it in its own domain.

## The ones you are most likely to hit

**2006 — hugepage-backed guest memory could not be mapped.** Almost always
an empty 1 GiB hugepage pool rather than a genuine mapping fault. The message
names the number of pages the machine needs. Reserving them at runtime works
until memory fragments; reserve at boot with
`default_hugepagesz=1G hugepagesz=1G hugepages=8`. See
[HOST-REQUIREMENTS.md](../HOST-REQUIREMENTS.md).

**2007 — split irqchip could not be enabled.** §1.4 forbids a legacy PIC/PIT,
so this is fatal by design rather than something to fall back from. The host
kernel needs `KVM_CAP_SPLIT_IRQCHIP`.

**1001 — `deny_unknown_fields` rejected a key.** The schema is closed, so a
typo in a key name aborts the boot instead of being silently ignored. The
message names the key and the section it was found in.

**1020 — TPM state must never be lost, so a volatile engine is refused.**
Configuring `tpm.storage` on `rust_hugepage_file` means the TPM's state
disappears on shutdown, which would silently break measured boot and
BitLocker-style key sealing. Point it at a persistent engine.

**5011 — no codec in common.** Raised when the client's
`X-Codec-Capabilities` header and the server's encode capability do not
intersect. The message lists both sets. A client that sends no header at all
never hits this: it is assumed to want H.264 and Vorbis. See
[Revision B §7.6](spec-revision-B-media-codecs.md).

**5001 — the hardware encoder could not be initialised.** Note the
distinction the media layer draws: a host that *has* no hardware encoder for
the negotiated codec falls back to software silently and by design, and does
**not** raise this. 5001 means an encoder that the driver said exists then
failed to open, which is a real fault — degrading silently there would hide
it behind a performance regression nobody would attribute correctly.

**5012 — the console could not be put on screen.** Raised by `--display`
when there is no Wayland compositor to talk to, when the compositor offers
no `xdg_wm_base` or `wl_shm`, or when a frame arrives in a format the window
cannot blit. The message names `WAYLAND_DISPLAY` and `XDG_RUNTIME_DIR` for
the first case, which is nearly always the real one.

**8005 — quiesce timed out.** Non-fatal. The backup proceeds
crash-consistent and logs this as a warning; it is recorded here because it
appears in logs looking like a failure.

**4002 — the engine could not be opened at `DEVICE_INIT`.** For
`rust_nvme` this is expected: that engine is a capability contract with no
datapath, and it fails loudly rather than letting a machine run on a stub.
For `rust_ceph_rbd` it usually means the cluster is unreachable, and the
message names the cause.

## Full catalogue

Generated from `crates/libvmm-core/src/error.rs` and
`crates/libvmm-config/src/error.rs`, which remain the authority.

### 1xxx — Config

`VmmError::Config` wrapping `ConfigError`.

| code | variant | meaning |
|---|---|---|
| **1000** | `Malformed` | TOML syntax or type error. |
| **1001** | `UnknownKey` | `deny_unknown_fields` rejected a key. Boot MUST abort. |
| **1002** | `BadMemoryMultiple` | total RAM is not a whole multiple of 1 GiB (§1.3). |
| **1003** | `LowRamOverlapsMmioHole` | low RAM would overlap the MMIO hole (§1.3 invariant). |
| **1004** | `RamSplitMismatch` | low + high does not add up to the declared total. |
| **1005** | `VmNameLength` | SMBIOS Type 1 serial constraint (§3.2). |
| **1006** | `VmIdNotUuid` | `vm.id` is not a UUID. |
| **1007** | `NoVcpus` | at least one vCPU is required; it also fixes the queue count. |
| **1008** | `Io` | the configuration file could not be read. |
| **1010** | `AcpiChecksum` | a generated or injected table failed its 8-bit sum-to-zero check. |
| **1020** | `TpmVolatileEngine` | TPM state must never be lost, so a volatile engine is refused. |
| **1030** | `EngineMissingField` | an engine is missing a field it requires. |
| **1031** | `EngineUnexpectedField` | an engine was given a field belonging to a different engine. |
| **1040** | `DuplicateDriveId` | duplicate `drive_id`. |
| **1041** | `DuplicateSocketPath` | duplicate vhost-user socket path across drives/cards. |
| **1042** | `BootableDriveCount` | more than one bootable drive, or none. |
| **1050** | `BitrateAboveCeiling` | VBR target must stay strictly below the hard ceiling (item 10). |
| **1051** | `BitrateCeilingTooHigh` | the 2000 kbps ceiling is a hard cap, not a suggestion. |
| **1052** | `AudioFormat` | audio capture format is fixed at 48 kHz S16LE stereo (§7.1). |
| **1060** | `TlsVersionNotSupported` | TLS 1.3 only, no fallback (change-log item 3). |
| **1061** | `MaxClientsOutOfRange` | the WSS client cap is a hard 2 (change-log item 11). |
| **1062** | `PortConflict` | two listeners cannot share a TCP port. |
| **1063** | `EmptyCredential` | auth is required but credentials are empty. |
| **1070** | `BusConflict` | two subsystems claimed the same PCIe bus. |
| **1071** | `PeripheralSlotConflict` | two peripherals claimed the same slot on the peripheral bus. |
| **1080** | `BadMacAddress` | malformed MAC address. |
| **1081** | `BadUsbipEndpoint` | the USB/IP server endpoint is not a socket address. |
| **1082** | `UsbipPortTlsMismatch` | `use_tls` selects :3241, cleartext is :3240 (§9). |
| **1090** | `ZstdLevelOutOfRange` | zstd level out of range. |

### 2xxx — KVM

`VmmError::Kvm` wrapping `KvmError`.

| code | variant | meaning |
|---|---|---|
| **2000** | `OpenDevice` | /dev/kvm could not be opened. |
| **2001** | `CreateVm` | KVM_CREATE_VM failed. |
| **2002** | `CreateVcpu` | KVM_CREATE_VCPU failed. |
| **2003** | `ApiVersion` | the host KVM API version is not the one we build against. |
| **2004** | `MissingCapability` | a required KVM capability is missing. |
| **2005** | `SetMemRegion` | KVM_SET_USER_MEMORY_REGION failed. |
| **2006** | `MemoryMap` | hugepage-backed guest memory could not be mapped. |
| **2007** | `SplitIrqchip` | split irqchip could not be enabled; a legacy PIC/PIT would be required, which §1.4 forbids. |
| **2008** | `GsiRouting` | MSI GSI routing setup failed. |
| **2009** | `VcpuState` | CPUID/MSR/SREGS programming failed. |
| **2010** | `VcpuRun` | KVM_RUN returned an error or an exit we cannot service. |
| **2011** | `EventFd` | an eventfd/irqfd registration failed. |
| **2012** | `FirmwareLoad` | firmware image could not be loaded into its slot. |

### 3xxx — Virtio

`VmmError::Virtio` wrapping `VirtioError`.

| code | variant | meaning |
|---|---|---|
| **3001** | `FeatureMismatch` | the guest cleared FEATURES_OK; the device MUST refuse to run. |
| **3002** | `BadStatusTransition` | the device status handshake was driven out of order. |
| **3005** | `BadDescriptor` | a descriptor chain is malformed or points outside guest RAM. |
| **3006** | `NoSuchQueue` | the driver selected a queue that does not exist. |
| **3007** | `BadQueueSize` | the driver programmed an unusable ring size. |
| **3008** | `QueueNotConfigured` | a queue was enabled before its ring addresses were programmed. |

### 4xxx — Storage

`VmmError::Storage` wrapping `StorageError`.

| code | variant | meaning |
|---|---|---|
| **4001** | `BackendLost` | a vhost-user back-end closed its socket (§5.6). The VMM MUST NOT panic: the drive is marked failed and in-flight requests get CHECK CONDITION. |
| **4002** | `EngineOpen` | the engine could not be opened at DEVICE_INIT. |
| **4005** | `EngineIo` | an I/O submission or completion failed. |
| **4006** | `LbaOutOfRange` | a request addressed an LBA past the end of the image. |
| **4010** | `UnmapUnsupported` | UNMAP was issued to an engine with no discard support. |
| **4011** | `UnsupportedCdb` | the guest sent a CDB we do not implement. |
| **4012** | `SnapshotUnsupported` | the engine cannot take a snapshot (surfaces as 8001 in backup). |

### 5xxx — Media

`VmmError::Media` wrapping `MediaError`.

| code | variant | meaning |
|---|---|---|
| **5001** | `EncoderInit` | the hardware encoder could not be initialised. |
| **5002** | `AudioEncoderInit` | the audio encoder could not be initialised. |
| **5003** | `BadRequest` | an RTSP request was malformed. |
| **5004** | `BadState` | a method arrived in a state that does not accept it (§7.2). |
| **5005** | `RtspAuth` | Basic Auth missing or invalid; answered 401 before any encoder resource is allocated (§7.4). |
| **5006** | `NoSuchStream` | the requested stream path does not exist. |
| **5007** | `Tls` | TLS setup failed on the RTSPS listener. |
| **5008** | `Packetize` | a frame could not be packetised for RTP (§7.3). |
| **5009** | `Encode` | a frame could not be encoded (§7.1). |
| **5010** | `Capture` | captured frame geometry did not match the encoder (§7.1). |
| **5011** | `NoCommonCodec` | server and client share no codec for a stream (§7.6). The message names both sets: a client cannot fix a mismatch it cannot see. |
| **5012** | `Display` | the client could not put decoded frames on screen. Distinct from 5010: that is a frame the encoder cannot accept, this is a display surface that will not take one. |

### 6xxx — Control

`VmmError::Control` wrapping `ControlError`.

| code | variant | meaning |
|---|---|---|
| **6000** | `Bind` | the listener could not bind. |
| **6001** | `Tls` | TLS setup or certificate generation failed. |
| **6002** | `BadUpgrade` | the WebSocket upgrade handshake was malformed. |
| **6003** | `AtClientCap` | at the client cap; rejected 503 before upgrade (§8.3). |
| **6004** | `LockedOut` | source IP is locked out; answered 429 with no credential check. |
| **6005** | `Connect` | a client could not reach the control endpoint. |
| **6400** | `BadFrame` | malformed JSON, unknown action, or mismatched `v`. |
| **6401** | `NotAuthenticated` | not authenticated (should not occur post-upgrade). |
| **6422** | `InputRange` | input payload out of range (bad keycode / coordinate). |
| **6423** | `BackupBusy` | a backup is already running. |
| **6500** | `Internal` | internal error executing an action. |

### 7xxx — USB/IP

`VmmError::Usbip` wrapping `UsbipError`.

| code | variant | meaning |
|---|---|---|
| **7000** | `Connect` | could not connect to the USB/IP server. |
| **7001** | `ImportDenied` | the server refused an import (busid not in its allow-set). |
| **7002** | `BadWire` | a wire structure was truncated or had a bad version/code. |
| **7005** | `PeerReset` | the peer reset the connection; the device is detached and the guest gets an xHCI port-disconnect event. |
| **7006** | `NoFreePort` | no free virtual xHCI port. |
| **7007** | `Tls` | TLS error on the :3241 transport. |

### 8xxx — Backup

`VmmError::Backup` wrapping `BackupError`.

| code | variant | meaning |
|---|---|---|
| **8001** | `EngineNotSnapshotCapable` | an included drive sits on a non-snapshot engine. PREFLIGHT aborts rather than produce an inconsistent copy (§10.2). |
| **8002** | `Snapshot` | taking a snapshot failed. |
| **8005** | `QuiesceTimeout` | quiesce timed out. Non-fatal: the backup proceeds crash-consistent and logs this as a warning (§10.1 step 3). |
| **8010** | `StreamIo` | writing the .vmbk stream failed. |
| **8011** | `AlreadyRunning` | a backup is already running (mirrors control 6423). |

## Adding a code

1. Add the variant to the right enum with a `///` doc comment that starts
   with the number, and a `#[error("…")]` message that names the offending
   value — the message is what an operator reads first.
2. Add the arm to that enum's `code()`.
3. If the condition is one an operator can act on, add it to *The ones you
   are most likely to hit* above with the remedy.

Reuse of a retired number is not permitted: logs and tickets outlive the
code that produced them.
