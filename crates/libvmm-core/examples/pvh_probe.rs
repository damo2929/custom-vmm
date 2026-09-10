//! Does this image carry a PVH entry point?
//!
//! Answers, for any firmware or kernel blob, whether this VMM's PVH loader
//! can start it — which for a firmware image is the difference between
//! "load it" and "emulate a chipset first".
fn main() {
    for path in std::env::args().skip(1) {
        match std::fs::read(&path) {
            Err(e) => println!("{path}: unreadable: {e}"),
            Ok(image) => {
                // `first_chunk` would read better but is stable only from
                // 1.77 and this workspace's MSRV is 1.75. A short file must
                // not panic here — the whole point of this program is to be
                // pointed at files that may be anything at all.
                let mut head = [0u8; 4];
                for (slot, byte) in head.iter_mut().zip(image.iter()) {
                    *slot = *byte;
                }
                let kind = if head == *b"\x7fELF" {
                    "ELF"
                } else {
                    "not ELF"
                };
                match libvmm_core::pvh::parse(&image) {
                    Ok(k) => println!(
                        "{path}: {kind}, PVH entry {:#x}, loads {:#x}..{:#x}",
                        k.entry,
                        k.base(),
                        k.top()
                    ),
                    Err(e) => println!("{path}: {kind}, no PVH ({e})"),
                }
            }
        }
    }
}
