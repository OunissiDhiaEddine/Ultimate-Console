// Shared by the apps' build.rs files via `include!`. Embeds packaging/icon.ico
// as the .exe icon (resource 1) so Explorer, the taskbar and Alt+Tab show the
// logo. It is a static resource: nothing is drawn at runtime.
//
// No resource compiler is needed: the .ico is turned into a .res file here
// and handed straight to the MSVC linker, which accepts .res inputs.

fn embed_app_icon() {
    use std::path::PathBuf;

    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let ico_path = manifest.join("../../packaging/icon.ico");
    println!("cargo:rerun-if-changed={}", ico_path.display());

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os != "windows" || target_env != "msvc" {
        return;
    }

    let ico = std::fs::read(&ico_path).expect("read packaging/icon.ico");
    let res = ico_to_res(&ico);
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("app-icon.res");
    std::fs::write(&out, res).expect("write app-icon.res");
    println!("cargo:rustc-link-arg-bins={}", out.display());
}

/// Converts an .ico file into a Win32 .res holding one RT_ICON per image and
/// an RT_GROUP_ICON with id 1 that lists them.
fn ico_to_res(ico: &[u8]) -> Vec<u8> {
    const RT_ICON: u16 = 3;
    const RT_GROUP_ICON: u16 = 14;
    let u16_at = |o: usize| u16::from_le_bytes([ico[o], ico[o + 1]]);
    let u32_at = |o: usize| u32::from_le_bytes([ico[o], ico[o + 1], ico[o + 2], ico[o + 3]]);

    assert!(ico.len() >= 6 && u16_at(0) == 0 && u16_at(2) == 1, "not an .ico file");
    let count = u16_at(4) as usize;

    let mut res = Vec::new();
    // A .res file starts with an empty entry.
    push_entry(&mut res, 0, 0, 0, &[]);

    let mut group = Vec::new();
    group.extend_from_slice(&0u16.to_le_bytes());
    group.extend_from_slice(&1u16.to_le_bytes());
    group.extend_from_slice(&(count as u16).to_le_bytes());
    for i in 0..count {
        let e = 6 + 16 * i;
        let size = u32_at(e + 8) as usize;
        let offset = u32_at(e + 12) as usize;
        let id = (i + 1) as u16;
        push_entry(&mut res, RT_ICON, id, 0x1010, &ico[offset..offset + size]);
        // GRPICONDIRENTRY: the ICONDIRENTRY's first 12 bytes, then the id.
        group.extend_from_slice(&ico[e..e + 12]);
        group.extend_from_slice(&id.to_le_bytes());
    }
    push_entry(&mut res, RT_GROUP_ICON, 1, 0x1030, &group);
    res
}

fn push_entry(res: &mut Vec<u8>, ty: u16, name: u16, flags: u16, data: &[u8]) {
    res.extend_from_slice(&(data.len() as u32).to_le_bytes()); // DataSize
    res.extend_from_slice(&32u32.to_le_bytes()); // HeaderSize
    res.extend_from_slice(&[0xFF, 0xFF]);
    res.extend_from_slice(&ty.to_le_bytes());
    res.extend_from_slice(&[0xFF, 0xFF]);
    res.extend_from_slice(&name.to_le_bytes());
    res.extend_from_slice(&0u32.to_le_bytes()); // DataVersion
    res.extend_from_slice(&flags.to_le_bytes()); // MemoryFlags
    let lang: u16 = if ty == 0 { 0 } else { 0x0409 }; // en-US
    res.extend_from_slice(&lang.to_le_bytes()); // LanguageId
    res.extend_from_slice(&0u32.to_le_bytes()); // Version
    res.extend_from_slice(&0u32.to_le_bytes()); // Characteristics
    res.extend_from_slice(data);
    while !res.len().is_multiple_of(4) {
        res.push(0);
    }
}
