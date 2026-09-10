//! Write the generated ACPI tables to files, so `iasl -d` can check them.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".to_string());
    let cfg = libvmm_config::MachineConfig::from_toml_str(include_str!(
        "../../../config/reference-vm.toml"
    ))?;
    let map = libvmm_core::memory::GuestMemoryMap::new(&cfg.memory)?;
    let set = libvmm_core::acpi::builder::build(&cfg, &map, &|_p| Ok(vec![]))?;
    for t in &set.tables {
        let path = format!("{dir}/{}.aml", t.signature);
        std::fs::write(&path, &t.bytes)?;
        println!("{path} ({} bytes)", t.bytes.len());
    }
    Ok(())
}
