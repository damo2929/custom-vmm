//! Host USB access — sysfs enumeration and usbdevfs URB submission.
//!
//! §9.2 step 3 has the server "unbind the local kernel driver, bind
//! usbip-host". This implementation claims the interface through usbdevfs
//! instead, which achieves the same isolation — the kernel driver is detached
//! for as long as the device is exported — without requiring the `usbip-host`
//! module to be present.
//!
//! Everything here is pure Rust over `libc` ioctls; no C library is linked
//! (§1.1).

use libvmm_core::{UsbipError, VmmResult};
use libvmm_usbip::wire::DeviceInfo;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

const SYSFS_USB_DEVICES: &str = "/sys/bus/usb/devices";

// usbdevfs ioctls (linux/usbdevice_fs.h).
const USBDEVFS_SUBMITURB: libc::c_ulong = 0x8038550A;
const USBDEVFS_DISCARDURB: libc::c_ulong = 0x0000550B;
const USBDEVFS_REAPURBNDELAY: libc::c_ulong = 0x4008550D;
const USBDEVFS_CLAIMINTERFACE: libc::c_ulong = 0x8004550F;
const USBDEVFS_RELEASEINTERFACE: libc::c_ulong = 0x80045510;
const USBDEVFS_DISCONNECT_CLAIM: libc::c_ulong = 0x8108551B;

const USBDEVFS_URB_TYPE_CONTROL: u8 = 2;
const USBDEVFS_URB_TYPE_BULK: u8 = 3;
const USBDEVFS_URB_TYPE_INTERRUPT: u8 = 1;

const USBDEVFS_DISCONNECT_CLAIM_EXCEPT_DRIVER: u32 = 0x02;

/// `struct usbdevfs_urb`, as the kernel defines it.
#[repr(C)]
struct UsbdevfsUrb {
    urb_type: libc::c_uchar,
    endpoint: libc::c_uchar,
    status: libc::c_int,
    flags: libc::c_uint,
    buffer: *mut libc::c_void,
    buffer_length: libc::c_int,
    actual_length: libc::c_int,
    start_frame: libc::c_int,
    number_of_packets_or_stream_id: libc::c_int,
    error_count: libc::c_int,
    signr: libc::c_uint,
    usercontext: *mut libc::c_void,
    // Isochronous descriptors follow; we submit none.
}

/// `struct usbdevfs_disconnect_claim`.
#[repr(C)]
struct DisconnectClaim {
    interface: libc::c_uint,
    flags: libc::c_uint,
    driver: [libc::c_char; 256],
}

/// A USB device present on this host.
#[derive(Debug, Clone)]
pub struct HostDevice {
    pub busid: String,
    pub busnum: u32,
    pub devnum: u32,
    pub id_vendor: u16,
    pub id_product: u16,
    pub bcd_device: u16,
    pub device_class: u8,
    pub device_subclass: u8,
    pub device_protocol: u8,
    pub num_configurations: u8,
    pub num_interfaces: u8,
    pub configuration_value: u8,
    pub speed: u32,
    pub sysfs_path: PathBuf,
}

impl HostDevice {
    /// The `/dev/bus/usb/BBB/DDD` node for this device.
    pub fn devnode(&self) -> PathBuf {
        PathBuf::from(format!(
            "/dev/bus/usb/{:03}/{:03}",
            self.busnum, self.devnum
        ))
    }

    /// The USB/IP wire description of this device (§9.1).
    pub fn to_wire(&self) -> DeviceInfo {
        DeviceInfo {
            path: self.sysfs_path.display().to_string(),
            busid: self.busid.clone(),
            busnum: self.busnum,
            devnum: self.devnum,
            speed: self.speed,
            id_vendor: self.id_vendor,
            id_product: self.id_product,
            bcd_device: self.bcd_device,
            device_class: self.device_class,
            device_subclass: self.device_subclass,
            device_protocol: self.device_protocol,
            configuration_value: self.configuration_value,
            num_configurations: self.num_configurations,
            num_interfaces: self.num_interfaces,
        }
    }
}

/// Enumerate the USB devices this host exposes.
///
/// Reads sysfs, which needs no privileges — so a devlist works even where
/// opening the device node would not.
pub fn enumerate() -> Vec<HostDevice> {
    let Ok(entries) = std::fs::read_dir(SYSFS_USB_DEVICES) else {
        return Vec::new();
    };
    let mut devices = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        // Interface directories look like `1-2:1.0`; skip them and the roots.
        if name.contains(':') || name.starts_with("usb") {
            continue;
        }
        if let Some(d) = read_device(&path, &name) {
            devices.push(d);
        }
    }
    devices.sort_by(|a, b| a.busid.cmp(&b.busid));
    devices
}

fn read_device(path: &Path, busid: &str) -> Option<HostDevice> {
    let attr = |name: &str| -> Option<String> {
        std::fs::read_to_string(path.join(name))
            .ok()
            .map(|s| s.trim().to_string())
    };
    let hex = |name: &str| -> u16 {
        attr(name)
            .and_then(|v| u16::from_str_radix(&v, 16).ok())
            .unwrap_or(0)
    };
    let dec_u8 = |name: &str| -> u8 { attr(name).and_then(|v| v.parse().ok()).unwrap_or(0) };
    let class = |name: &str| -> u8 {
        attr(name)
            .and_then(|v| u8::from_str_radix(&v, 16).ok())
            .unwrap_or(0)
    };

    // A real device always has these; their absence means this is not one.
    let busnum = attr("busnum")?.parse().ok()?;
    let devnum = attr("devnum")?.parse().ok()?;

    Some(HostDevice {
        busid: busid.to_string(),
        busnum,
        devnum,
        id_vendor: hex("idVendor"),
        id_product: hex("idProduct"),
        bcd_device: attr("bcdDevice")
            .and_then(|v| u16::from_str_radix(&v.replace('.', ""), 16).ok())
            .unwrap_or(0),
        device_class: class("bDeviceClass"),
        device_subclass: class("bDeviceSubClass"),
        device_protocol: class("bDeviceProtocol"),
        num_configurations: dec_u8("bNumConfigurations"),
        num_interfaces: dec_u8("bNumInterfaces"),
        configuration_value: attr("bConfigurationValue")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1),
        speed: speed_code(&attr("speed").unwrap_or_default()),
        sysfs_path: path.to_path_buf(),
    })
}

/// USB/IP encodes speed as an enum, not Mbit/s.
fn speed_code(sysfs_speed: &str) -> u32 {
    match sysfs_speed {
        "1.5" => 1,   // low
        "12" => 2,    // full
        "480" => 3,   // high
        "5000" => 5,  // super
        "10000" => 6, // super plus
        _ => 0,       // unknown
    }
}

/// An opened host device, with its interfaces claimed away from their kernel
/// drivers for the duration.
pub struct ClaimedDevice {
    file: File,
    claimed: Vec<u32>,
    pub busid: String,
}

impl ClaimedDevice {
    /// Open `/dev/bus/usb/...` and detach the kernel driver from every
    /// interface, which is the isolation §9.2 step 3 describes.
    pub fn open(device: &HostDevice) -> VmmResult<Self> {
        let node = device.devnode();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&node)
            .map_err(|e| -> libvmm_core::VmmError {
                UsbipError::Connect {
                    addr: node.display().to_string(),
                    detail: format!(
                        "{e} (write access to the device node is required to export it)"
                    ),
                }
                .into()
            })?;

        let mut claimed = Vec::new();
        for interface in 0..device.num_interfaces.max(1) as u32 {
            if claim_interface(&file, interface) {
                claimed.push(interface);
            }
        }
        if claimed.is_empty() {
            return Err(UsbipError::ImportDenied {
                busid: format!("{} (no interface could be claimed)", device.busid),
            }
            .into());
        }
        Ok(ClaimedDevice {
            file,
            claimed,
            busid: device.busid.clone(),
        })
    }

    /// Submit one URB and wait for it to complete.
    ///
    /// `buffer` is the transfer buffer: filled by the device for an IN
    /// transfer, sent to it for an OUT.
    pub fn transfer(
        &self,
        endpoint: u8,
        is_control: bool,
        is_interrupt: bool,
        setup: &[u8; 8],
        buffer: &mut [u8],
    ) -> VmmResult<i32> {
        // A control transfer's buffer must begin with the 8-byte setup
        // packet, so build a combined buffer for that case.
        let mut control_buffer: Vec<u8> = Vec::new();
        let (ptr, len) = if is_control {
            control_buffer.reserve(8 + buffer.len());
            control_buffer.extend_from_slice(setup);
            control_buffer.extend_from_slice(buffer);
            (control_buffer.as_mut_ptr(), control_buffer.len())
        } else {
            (buffer.as_mut_ptr(), buffer.len())
        };

        let urb_type = if is_control {
            USBDEVFS_URB_TYPE_CONTROL
        } else if is_interrupt {
            USBDEVFS_URB_TYPE_INTERRUPT
        } else {
            USBDEVFS_URB_TYPE_BULK
        };

        let mut urb = UsbdevfsUrb {
            urb_type,
            endpoint,
            status: 0,
            flags: 0,
            buffer: ptr.cast(),
            buffer_length: len as libc::c_int,
            actual_length: 0,
            start_frame: 0,
            number_of_packets_or_stream_id: 0,
            error_count: 0,
            signr: 0,
            usercontext: std::ptr::null_mut(),
        };

        // SAFETY: `urb` is a correctly shaped usbdevfs_urb whose buffer
        // pointer refers to a live allocation of `buffer_length` bytes, and
        // the fd is an owned, open device node.
        let rc = unsafe { libc::ioctl(self.file.as_raw_fd(), USBDEVFS_SUBMITURB, &mut urb) };
        if rc < 0 {
            return Err(urb_error("USBDEVFS_SUBMITURB", &self.busid));
        }

        // Reap, polling briefly: NDELAY returns EAGAIN until the URB lands.
        let mut reaped: *mut UsbdevfsUrb = std::ptr::null_mut();
        for _ in 0..2000 {
            // SAFETY: writes a pointer to the completed URB into `reaped`.
            let rc =
                unsafe { libc::ioctl(self.file.as_raw_fd(), USBDEVFS_REAPURBNDELAY, &mut reaped) };
            if rc == 0 {
                break;
            }
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EAGAIN) {
                return Err(urb_error("USBDEVFS_REAPURBNDELAY", &self.busid));
            }
            std::thread::sleep(std::time::Duration::from_micros(500));
        }
        if reaped.is_null() {
            // SAFETY: discarding the URB we submitted, still owned here.
            unsafe { libc::ioctl(self.file.as_raw_fd(), USBDEVFS_DISCARDURB, &mut urb) };
            return Err(UsbipError::PeerReset(format!("URB on {} timed out", self.busid)).into());
        }

        let actual = urb.actual_length;
        if is_control {
            // Copy the device's reply back past the setup packet.
            let copied = (actual as usize).min(buffer.len());
            buffer[..copied].copy_from_slice(&control_buffer[8..8 + copied]);
        }
        Ok(actual)
    }
}

impl Drop for ClaimedDevice {
    fn drop(&mut self) {
        for interface in &self.claimed {
            // SAFETY: releasing an interface this handle claimed.
            unsafe {
                libc::ioctl(self.file.as_raw_fd(), USBDEVFS_RELEASEINTERFACE, interface);
            }
        }
    }
}

/// Detach any kernel driver and claim the interface.
fn claim_interface(file: &File, interface: u32) -> bool {
    let mut request = DisconnectClaim {
        interface,
        flags: USBDEVFS_DISCONNECT_CLAIM_EXCEPT_DRIVER,
        driver: [0; 256],
    };
    // SAFETY: a correctly shaped usbdevfs_disconnect_claim on an owned fd.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), USBDEVFS_DISCONNECT_CLAIM, &mut request) };
    if rc == 0 {
        return true;
    }
    // Older kernels lack DISCONNECT_CLAIM; fall back to a plain claim.
    // SAFETY: as above.
    unsafe { libc::ioctl(file.as_raw_fd(), USBDEVFS_CLAIMINTERFACE, &interface) == 0 }
}

fn urb_error(op: &str, busid: &str) -> libvmm_core::VmmError {
    UsbipError::PeerReset(format!(
        "{op} on {busid}: {}",
        std::io::Error::last_os_error()
    ))
    .into()
}

/// Read a device's raw descriptors, which a client may request before it has
/// issued any control transfer.
pub fn read_descriptors(device: &HostDevice) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    File::open(device.sysfs_path.join("descriptors"))
        .ok()?
        .read_to_end(&mut buf)
        .ok()?;
    Some(buf)
}
