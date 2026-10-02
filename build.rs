#[allow(dead_code)] // the tray glyph is only used at runtime
#[path = "src/icon.rs"]
mod icon;

use std::path::{Path, PathBuf};

/// Sizes embedded for Explorer. The app's own windows render the icon at
/// runtime at the exact size they need, so only the common sizes live here
/// (large Explorer views scale up from 48).
const ICON_SIZES: [u32; 3] = [16, 32, 48];
const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
/// Resource id of the icon group (Explorer uses the first group).
const ICON_GROUP_ID: u16 = 1;

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = root.join("bufferless.manifest");
    println!("cargo:rerun-if-changed={}", manifest.display());
    println!("cargo:rerun-if-changed=src/icon.rs");

    // Embed the manifest and icon with the MSVC linker directly; no resource
    // compiler needed.
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}", manifest.display());
    let res = write_icon_res();
    println!("cargo:rustc-link-arg-bins={}", res.display());
}

fn png(size: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, size, size);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::High);
    let mut writer = encoder.write_header().unwrap();
    writer.write_image_data(&icon::tile(size)).unwrap();
    writer.finish().unwrap();
    out
}

/// Append one entry in the binary .res format (RESOURCEHEADER + data).
fn resource(res: &mut Vec<u8>, kind: u16, id: u16, flags: u16, data: &[u8]) {
    let u16le = |res: &mut Vec<u8>, v: u16| res.extend_from_slice(&v.to_le_bytes());
    let u32le = |res: &mut Vec<u8>, v: u32| res.extend_from_slice(&v.to_le_bytes());
    u32le(res, data.len() as u32);
    u32le(res, 32); // header size
    u16le(res, 0xffff);
    u16le(res, kind);
    u16le(res, 0xffff);
    u16le(res, id);
    u32le(res, 0); // data version
    u16le(res, flags);
    u16le(res, 0x0409); // en-US
    u32le(res, 0); // version
    u32le(res, 0); // characteristics
    res.extend_from_slice(data);
    while res.len() % 4 != 0 {
        res.push(0);
    }
}

fn write_icon_res() -> PathBuf {
    let mut res = Vec::new();
    // A .res file starts with an empty entry.
    resource(&mut res, 0, 0, 0, &[]);

    // Each size becomes an RT_ICON (PNG-compressed images are fine for every
    // size since Vista), and an RT_GROUP_ICON directory ties them together.
    let mut group = Vec::new();
    group.extend_from_slice(&0u16.to_le_bytes());
    group.extend_from_slice(&1u16.to_le_bytes()); // type: icon
    group.extend_from_slice(&(ICON_SIZES.len() as u16).to_le_bytes());
    for (i, &size) in ICON_SIZES.iter().enumerate() {
        let id = i as u16 + 1;
        let image = png(size);
        resource(&mut res, RT_ICON, id, 0x1010, &image);
        let dim = if size >= 256 { 0 } else { size as u8 };
        group.extend_from_slice(&[dim, dim, 0, 0]);
        group.extend_from_slice(&1u16.to_le_bytes()); // planes
        group.extend_from_slice(&32u16.to_le_bytes()); // bit count
        group.extend_from_slice(&(image.len() as u32).to_le_bytes());
        group.extend_from_slice(&id.to_le_bytes());
    }
    resource(&mut res, RT_GROUP_ICON, ICON_GROUP_ID, 0x1030, &group);

    let path = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("icon.res");
    std::fs::write(&path, res).unwrap();
    path
}
