//! The modern virtio-pci transport (virtio 1.x, §4.1).
//!
//! One 64-bit MMIO BAR carries four register regions, laid out by
//! [`BarLayout`]: common configuration, per-queue notification doorbells,
//! the ISR byte, and the device-specific configuration. This module is the
//! register file behind that BAR — it owns feature negotiation, the status
//! byte, and the queue descriptors, and hands the device nothing but
//! "queue `n` was kicked".
//!
//! Legacy virtio is not implemented and will not be: §1.4 requires
//! `VIRTIO_F_VERSION_1`, and Linux's virtio-gpu driver refuses to probe
//! without it.
//!
//! Interrupts are MSI-X only. The ISR byte still exists because the
//! specification requires the region to be readable, but with a per-queue
//! MSI-X vector configured nothing consults it.

use crate::features::{ring_layout, RingLayout};
use crate::pci_cap::BarLayout;
use crate::queue::{GuestMemory, Virtqueue};
use crate::status::DeviceStatus;
use libvmm_core::VmmResult;

/// Common configuration register offsets (virtio 1.x §4.1.4.3).
mod common {
    pub const DEVICE_FEATURE_SELECT: u64 = 0x00;
    pub const DEVICE_FEATURE: u64 = 0x04;
    pub const DRIVER_FEATURE_SELECT: u64 = 0x08;
    pub const DRIVER_FEATURE: u64 = 0x0C;
    pub const MSIX_CONFIG: u64 = 0x10;
    pub const NUM_QUEUES: u64 = 0x12;
    pub const DEVICE_STATUS: u64 = 0x14;
    pub const CONFIG_GENERATION: u64 = 0x15;
    pub const QUEUE_SELECT: u64 = 0x16;
    pub const QUEUE_SIZE: u64 = 0x18;
    pub const QUEUE_MSIX_VECTOR: u64 = 0x1A;
    pub const QUEUE_ENABLE: u64 = 0x1C;
    pub const QUEUE_NOTIFY_OFF: u64 = 0x1E;
    pub const QUEUE_DESC: u64 = 0x20;
    pub const QUEUE_DRIVER: u64 = 0x28;
    pub const QUEUE_DEVICE: u64 = 0x30;
}

/// The MSI-X "no vector" sentinel.
pub const VIRTQ_MSI_NO_VECTOR: u16 = 0xFFFF;

/// One queue's transport-visible state.
pub struct QueueState {
    pub queue: Virtqueue,
    pub desc: u64,
    pub driver: u64,
    pub device: u64,
    pub size: u16,
    pub enabled: bool,
    pub msix_vector: u16,
    /// Set when the guest writes this queue's doorbell, cleared by
    /// [`VirtioTransport::take_kicks`].
    kicked: bool,
}

impl QueueState {
    fn new(index: u16, max_size: u16) -> Self {
        QueueState {
            queue: Virtqueue::new(index, max_size, RingLayout::Split),
            desc: 0,
            driver: 0,
            device: 0,
            size: max_size,
            enabled: false,
            msix_vector: VIRTQ_MSI_NO_VECTOR,
            kicked: false,
        }
    }
}

/// What a guest write to the BAR asked the device to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Nothing the device needs to know about.
    None,
    /// The guest kicked a queue's doorbell.
    Notify(u16),
    /// The guest set DRIVER_OK: the device may start.
    DriverOk,
    /// The guest wrote 0 to the status byte: reset everything.
    Reset,
    /// The guest gave up on the device.
    Failed,
}

/// The modern virtio-pci register file.
pub struct VirtioTransport {
    pub layout: BarLayout,
    /// Where the guest has mapped the BAR, if it has.
    pub bar_base: Option<u64>,
    /// Features this device offers.
    offered: u64,
    /// Features the driver accepted.
    acked: u64,
    device_feature_select: u32,
    driver_feature_select: u32,
    status: DeviceStatus,
    queue_select: u16,
    pub queues: Vec<QueueState>,
    max_queue_size: u16,
    config_msix_vector: u16,
    isr: u8,
    /// Bumped whenever device-specific configuration changes, so a guest
    /// reading it can tell it read a torn value.
    config_generation: u8,
    device_name: &'static str,
}

impl VirtioTransport {
    pub fn new(
        device_name: &'static str,
        num_queues: u16,
        max_queue_size: u16,
        device_cfg_len: u32,
        offered: u64,
    ) -> Self {
        VirtioTransport {
            layout: BarLayout::new(num_queues, device_cfg_len),
            bar_base: None,
            offered,
            acked: 0,
            device_feature_select: 0,
            driver_feature_select: 0,
            status: DeviceStatus::new(),
            queue_select: 0,
            queues: (0..num_queues)
                .map(|i| QueueState::new(i, max_queue_size))
                .collect(),
            max_queue_size,
            config_msix_vector: VIRTQ_MSI_NO_VECTOR,
            isr: 0,
            config_generation: 0,
            device_name,
        }
    }

    pub fn acked_features(&self) -> u64 {
        self.acked
    }

    pub fn driver_ok(&self) -> bool {
        self.status.is_running()
    }

    /// Note that device-specific configuration changed.
    pub fn bump_config_generation(&mut self) {
        self.config_generation = self.config_generation.wrapping_add(1);
    }

    /// Which queues the guest has kicked since this was last called.
    pub fn take_kicks(&mut self) -> Vec<u16> {
        let mut kicked = Vec::new();
        for (i, q) in self.queues.iter_mut().enumerate() {
            if q.kicked {
                q.kicked = false;
                kicked.push(i as u16);
            }
        }
        kicked
    }

    /// Does `addr` fall inside this device's BAR?
    pub fn contains(&self, addr: u64) -> bool {
        match self.bar_base {
            Some(base) => addr >= base && addr < base + self.layout.bar_size,
            None => false,
        }
    }

    fn offset_of(&self, addr: u64) -> Option<u64> {
        self.bar_base
            .filter(|base| addr >= *base && addr < *base + self.layout.bar_size)
            .map(|base| addr - base)
    }

    /// Read from the BAR. `device_config` is the device-specific region.
    pub fn read(&self, addr: u64, data: &mut [u8], device_config: &[u8]) {
        let Some(off) = self.offset_of(addr) else {
            data.fill(0);
            return;
        };
        let layout = &self.layout;

        if off < u64::from(layout.common_length) {
            let value = self.read_common(off);
            log::trace!("{}: common[{off:#04x}] -> {value:#x}", self.device_name);
            write_le(data, value);
            return;
        }
        let isr_start = u64::from(layout.isr_offset);
        if (isr_start..isr_start + u64::from(layout.isr_length)).contains(&off) {
            // Reading the ISR clears it, per §4.1.4.5. With MSI-X in use
            // nothing reads it, but the semantics have to be right for a
            // driver that probes.
            write_le(data, u64::from(self.isr));
            return;
        }
        let dev_start = u64::from(layout.device_offset);
        if (dev_start..dev_start + u64::from(layout.device_length)).contains(&off) {
            let idx = (off - dev_start) as usize;
            for (i, b) in data.iter_mut().enumerate() {
                *b = device_config.get(idx + i).copied().unwrap_or(0);
            }
            return;
        }
        // The notify region reads as zero; the MSI-X table and PBA are
        // handled by the PCI layer, not here.
        data.fill(0);
    }

    /// Reading the ISR clears it. Split out because [`read`] takes `&self`.
    pub fn read_isr_and_clear(&mut self) -> u8 {
        std::mem::take(&mut self.isr)
    }

    fn read_common(&self, off: u64) -> u64 {
        match off {
            common::DEVICE_FEATURE_SELECT => u64::from(self.device_feature_select),
            // Feature words beyond the 64 bits we model read as zero.
            // Linux walks the selector upwards until it has seen every
            // word the specification allows, so a device must answer a
            // select of 2 or 3 rather than assume it will never be asked.
            common::DEVICE_FEATURE => {
                u64::from(feature_word(self.offered, self.device_feature_select))
            }
            common::DRIVER_FEATURE_SELECT => u64::from(self.driver_feature_select),
            common::DRIVER_FEATURE => {
                u64::from(feature_word(self.acked, self.driver_feature_select))
            }
            common::MSIX_CONFIG => u64::from(self.config_msix_vector),
            common::NUM_QUEUES => self.queues.len() as u64,
            common::DEVICE_STATUS => u64::from(self.status.bits()),
            common::CONFIG_GENERATION => u64::from(self.config_generation),
            common::QUEUE_SELECT => u64::from(self.queue_select),
            common::QUEUE_SIZE => self.selected().map_or(0, |q| u64::from(q.size)),
            common::QUEUE_MSIX_VECTOR => self
                .selected()
                .map_or(u64::from(VIRTQ_MSI_NO_VECTOR), |q| u64::from(q.msix_vector)),
            common::QUEUE_ENABLE => self.selected().map_or(0, |q| u64::from(q.enabled)),
            // Every queue's doorbell is its index times the multiplier.
            common::QUEUE_NOTIFY_OFF => u64::from(self.queue_select),
            common::QUEUE_DESC => self.selected().map_or(0, |q| q.desc),
            common::QUEUE_DRIVER => self.selected().map_or(0, |q| q.driver),
            common::QUEUE_DEVICE => self.selected().map_or(0, |q| q.device),
            _ => 0,
        }
    }

    fn selected(&self) -> Option<&QueueState> {
        self.queues.get(self.queue_select as usize)
    }

    /// Write to the BAR. Returns what the device must act on.
    pub fn write(&mut self, addr: u64, data: &[u8]) -> Action {
        let Some(off) = self.offset_of(addr) else {
            return Action::None;
        };
        let value = read_le(data);
        let layout = &self.layout;

        if off < u64::from(layout.common_length) {
            return self.write_common(off, value);
        }

        let notify_start = u64::from(layout.notify_offset);
        if (notify_start..notify_start + u64::from(layout.notify_length)).contains(&off) {
            // The queue index is the doorbell's position, not the value
            // written: this device uses a per-queue address, which is what
            // `notify_off_multiplier != 0` promises the driver.
            let index = (off - notify_start) / u64::from(layout.notify_off_multiplier);
            if let Some(q) = self.queues.get_mut(index as usize) {
                q.kicked = true;
                return Action::Notify(index as u16);
            }
        }
        Action::None
    }

    fn write_common(&mut self, off: u64, value: u64) -> Action {
        log::trace!("{}: common[{off:#04x}] <- {value:#x}", self.device_name);
        match off {
            common::DEVICE_FEATURE_SELECT => self.device_feature_select = value as u32,
            common::DRIVER_FEATURE_SELECT => self.driver_feature_select = value as u32,
            common::DRIVER_FEATURE => {
                // A driver acking a bit we never offered, in a word we do
                // not model, is ignored rather than wrapped into word 0.
                if let Some(shift) = self
                    .driver_feature_select
                    .checked_mul(32)
                    .filter(|s| *s < 64)
                {
                    let mask = 0xFFFF_FFFFu64 << shift;
                    self.acked = (self.acked & !mask) | ((value & 0xFFFF_FFFF) << shift);
                }
            }
            common::MSIX_CONFIG => self.config_msix_vector = value as u16,
            common::DEVICE_STATUS => {
                let new = value as u8;
                if new == 0 {
                    self.reset();
                    return Action::Reset;
                }
                // A rejected transition is the driver's bug, not ours; log
                // it and keep the old status rather than pretending.
                if let Err(e) = self.status.write(self.device_name, new) {
                    log::warn!("{}: bad status transition: {e}", self.device_name);
                    return Action::None;
                }
                if self.status.bits() & crate::status::FAILED != 0 {
                    return Action::Failed;
                }
                if self.status.is_running() {
                    return Action::DriverOk;
                }
            }
            common::QUEUE_SELECT => self.queue_select = value as u16,
            common::QUEUE_SIZE => {
                let max = self.max_queue_size;
                if let Some(q) = self.queues.get_mut(self.queue_select as usize) {
                    // A size that is not a power of two, or is larger than
                    // the device offered, would make the ring arithmetic
                    // wrong. Clamp rather than trust it.
                    let size = (value as u16).min(max);
                    if size.is_power_of_two() {
                        q.size = size;
                    }
                }
            }
            common::QUEUE_MSIX_VECTOR => {
                if let Some(q) = self.queues.get_mut(self.queue_select as usize) {
                    q.msix_vector = value as u16;
                }
            }
            common::QUEUE_ENABLE => {
                let layout = ring_layout(self.acked);
                if let Some(q) = self.queues.get_mut(self.queue_select as usize) {
                    q.enabled = value & 1 != 0;
                    if q.enabled {
                        let index = q.queue.index;
                        q.queue = Virtqueue::new(index, q.size, layout);
                        q.queue.addresses = crate::queue::RingAddresses {
                            desc: q.desc,
                            driver: q.driver,
                            device: q.device,
                        };
                        if let Err(e) = q.queue.enable() {
                            log::warn!("{}: queue {index} refused: {e}", self.device_name);
                            q.enabled = false;
                        }
                    }
                }
            }
            common::QUEUE_DESC => self.set_queue_addr(|q| &mut q.desc, value),
            common::QUEUE_DRIVER => self.set_queue_addr(|q| &mut q.driver, value),
            common::QUEUE_DEVICE => self.set_queue_addr(|q| &mut q.device, value),
            _ => {}
        }
        Action::None
    }

    fn set_queue_addr(&mut self, which: fn(&mut QueueState) -> &mut u64, value: u64) {
        if let Some(q) = self.queues.get_mut(self.queue_select as usize) {
            *which(q) = value;
        }
    }

    fn reset(&mut self) {
        self.status = DeviceStatus::new();
        self.acked = 0;
        self.device_feature_select = 0;
        self.driver_feature_select = 0;
        self.queue_select = 0;
        self.isr = 0;
        let max = self.max_queue_size;
        for (i, q) in self.queues.iter_mut().enumerate() {
            *q = QueueState::new(i as u16, max);
        }
    }

    /// Pop a chain from queue `index`, if the guest has offered one.
    pub fn pop<M: GuestMemory>(
        &mut self,
        index: u16,
        mem: &M,
        chain: &mut crate::queue::DescriptorChain,
    ) -> VmmResult<bool> {
        match self.queues.get_mut(index as usize) {
            Some(q) if q.enabled => q.queue.pop(mem, chain),
            _ => Ok(false),
        }
    }

    /// Return a chain to the guest with `written` bytes filled in.
    pub fn push<M: GuestMemory>(
        &mut self,
        index: u16,
        mem: &M,
        chain: &crate::queue::DescriptorChain,
        written: u32,
    ) -> VmmResult<()> {
        match self.queues.get_mut(index as usize) {
            Some(q) if q.enabled => {
                self.isr |= 1;
                q.queue.push(mem, chain, written)
            }
            _ => Ok(()),
        }
    }

    /// Which MSI-X vector queue `index` was given, if any.
    pub fn queue_vector(&self, index: u16) -> Option<u16> {
        self.queues
            .get(index as usize)
            .map(|q| q.msix_vector)
            .filter(|v| *v != VIRTQ_MSI_NO_VECTOR)
    }
}

/// One 32-bit word of a 64-bit feature set, or zero past the end.
fn feature_word(features: u64, select: u32) -> u32 {
    match select.checked_mul(32) {
        Some(shift) if shift < 64 => ((features >> shift) & 0xFFFF_FFFF) as u32,
        _ => 0,
    }
}

fn read_le(data: &[u8]) -> u64 {
    let mut v = 0u64;
    for (i, b) in data.iter().take(8).enumerate() {
        v |= u64::from(*b) << (i * 8);
    }
    v
}

fn write_le(data: &mut [u8], value: u64) {
    for (i, b) in data.iter_mut().enumerate() {
        *b = if i < 8 { (value >> (i * 8)) as u8 } else { 0 };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transport() -> VirtioTransport {
        VirtioTransport::new("test", 2, 64, 16, crate::features::COMMON_OFFER)
    }

    /// A guest that asks for a feature word past the 64 we model must get
    /// zero, not a panic.
    ///
    /// This is not hypothetical. Linux walks the feature selector upwards
    /// past word 1, and `offered >> 64` is an overflow panic in a debug
    /// build. It killed the vCPU thread mid-MMIO, so the guest sat forever
    /// waiting for a read that would never be answered — a silent hang
    /// whose real cause was printed on a thread nobody was watching.
    #[test]
    fn a_feature_word_beyond_the_ones_we_model_reads_as_zero() {
        let mut t = transport();
        for select in 0..8u32 {
            t.write_common(common::DEVICE_FEATURE_SELECT, u64::from(select));
            let got = t.read_common(common::DEVICE_FEATURE);
            if select >= 2 {
                assert_eq!(got, 0, "feature word {select} must read as zero");
            }
        }
    }

    #[test]
    fn a_driver_acking_a_feature_word_we_do_not_model_is_ignored() {
        let mut t = transport();
        t.write_common(common::DRIVER_FEATURE_SELECT, 3);
        t.write_common(common::DRIVER_FEATURE, 0xFFFF_FFFF);
        assert_eq!(
            t.acked_features(),
            0,
            "an ack in word 3 must not wrap into word 0"
        );
    }

    #[test]
    fn the_two_low_feature_words_round_trip() {
        let mut t = transport();
        let offered = crate::features::COMMON_OFFER;
        t.write_common(common::DEVICE_FEATURE_SELECT, 0);
        assert_eq!(t.read_common(common::DEVICE_FEATURE), offered & 0xFFFF_FFFF);
        t.write_common(common::DEVICE_FEATURE_SELECT, 1);
        assert_eq!(t.read_common(common::DEVICE_FEATURE), offered >> 32);
    }

    #[test]
    fn a_doorbell_write_names_the_queue_by_its_address() {
        let mut t = transport();
        t.bar_base = Some(0xD000_0000);
        let base = 0xD000_0000;
        for queue in 0..2u16 {
            let doorbell = t.layout.doorbell_address(base, queue);
            // The value written is not the queue index — the address is.
            assert_eq!(t.write(doorbell, &[0xFF, 0xFF]), Action::Notify(queue));
        }
        assert_eq!(t.take_kicks(), vec![0, 1]);
        assert!(t.take_kicks().is_empty(), "kicks are consumed once");
    }

    #[test]
    fn writing_zero_to_the_status_byte_resets_everything() {
        let mut t = transport();
        t.bar_base = Some(0x1000);
        t.write_common(common::DEVICE_STATUS, u64::from(crate::status::ACKNOWLEDGE));
        t.write_common(common::DRIVER_FEATURE_SELECT, 1);
        t.write_common(common::DRIVER_FEATURE, 1);
        assert_ne!(t.acked_features(), 0);

        assert_eq!(t.write_common(common::DEVICE_STATUS, 0), Action::Reset);
        assert_eq!(t.acked_features(), 0);
        assert_eq!(t.read_common(common::DEVICE_STATUS), 0);
    }

    #[test]
    fn a_queue_size_that_is_not_a_power_of_two_is_refused() {
        let mut t = transport();
        t.write_common(common::QUEUE_SELECT, 0);
        t.write_common(common::QUEUE_SIZE, 63);
        assert_eq!(t.read_common(common::QUEUE_SIZE), 64, "63 is not accepted");
        t.write_common(common::QUEUE_SIZE, 32);
        assert_eq!(t.read_common(common::QUEUE_SIZE), 32);
        // A driver asking for more than the device offers is clamped to
        // the maximum rather than refused: the ring arithmetic only needs
        // a power of two that fits, and 64 is both.
        t.write_common(common::QUEUE_SIZE, 4096);
        assert_eq!(t.read_common(common::QUEUE_SIZE), 64);
    }

    #[test]
    fn an_access_outside_the_bar_is_ignored_rather_than_wrapping() {
        let mut t = transport();
        t.bar_base = Some(0xD000_0000);
        assert_eq!(t.write(0xC000_0000, &[1, 0, 0, 0]), Action::None);
        let mut data = [0xAAu8; 4];
        t.read(0xC000_0000, &mut data, &[]);
        assert_eq!(data, [0; 4]);
    }
}
