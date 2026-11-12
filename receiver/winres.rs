//! Turns assets/icon/tightwire.ico into a compiled Windows resource file
//! (.res) holding the exe's icon, and hands it to the linker.
//!
//! Written by hand instead of using `embed-resource`/`winresource` because
//! those need a resource compiler (rc.exe, windres or llvm-rc), and the
//! release exe is cross-linked from macOS with zig, where none of them is
//! available without extra installs. A .res with only RT_ICON and
//! RT_GROUP_ICON entries is a few fixed-layout headers, and both link.exe
//! (MSVC) and zig's linker (windows-gnu) accept .res inputs directly.

use std::path::Path;

const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
/// LANG_ENGLISH, SUBLANG_ENGLISH_US.
const LANG: u16 = 0x0409;
/// The exe's main icon: Explorer uses the group with the lowest id.
const GROUP_ID: u16 = 1;

/// For Windows targets: writes the .res to OUT_DIR and links it into the
/// tray/CLI binary only.
pub fn embed_icon(ico_path: &Path, out_dir: &Path, bin: &str) {
    println!("cargo:rerun-if-changed={}", ico_path.display());
    let ico =
        std::fs::read(ico_path).unwrap_or_else(|e| panic!("reading {}: {e}", ico_path.display()));
    let res = icon_res(&ico).unwrap_or_else(|e| panic!("{}: {e}", ico_path.display()));
    let res_path = out_dir.join("tightwire-icon.res");
    std::fs::write(&res_path, res).expect("writing the icon .res");
    println!("cargo:rustc-link-arg-bin={bin}={}", res_path.display());
}

struct IcoImage<'a> {
    width: u8,
    height: u8,
    colors: u8,
    planes: u16,
    bit_count: u16,
    data: &'a [u8],
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn parse_ico(ico: &[u8]) -> Result<Vec<IcoImage<'_>>, String> {
    if ico.len() < 6 || u16_at(ico, 0) != 0 || u16_at(ico, 2) != 1 {
        return Err("not an .ico file".into());
    }
    let count = u16_at(ico, 4) as usize;
    (0..count)
        .map(|i| {
            let e = 6 + 16 * i;
            let entry = ico.get(e..e + 16).ok_or("truncated directory")?;
            let (size, offset) = (u32_at(entry, 8) as usize, u32_at(entry, 12) as usize);
            let data = ico
                .get(offset..offset + size)
                .ok_or("image outside the file")?;
            Ok(IcoImage {
                width: entry[0],
                height: entry[1],
                colors: entry[2],
                planes: u16_at(entry, 4),
                bit_count: u16_at(entry, 6),
                data,
            })
        })
        .collect()
}

/// One RESOURCEHEADER (ordinal type and name) followed by the data, padded
/// to a DWORD boundary.
fn push_resource(out: &mut Vec<u8>, kind: u16, id: u16, flags: u16, data: &[u8]) {
    out.extend((data.len() as u32).to_le_bytes()); // DataSize
    out.extend(32u32.to_le_bytes()); // HeaderSize
    out.extend([0xFF, 0xFF]);
    out.extend(kind.to_le_bytes());
    out.extend([0xFF, 0xFF]);
    out.extend(id.to_le_bytes());
    out.extend(0u32.to_le_bytes()); // DataVersion
    out.extend(flags.to_le_bytes()); // MemoryFlags
    out.extend(LANG.to_le_bytes());
    out.extend(0u32.to_le_bytes()); // Version
    out.extend(0u32.to_le_bytes()); // Characteristics
    out.extend(data);
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

fn icon_res(ico: &[u8]) -> Result<Vec<u8>, String> {
    let images = parse_ico(ico)?;
    // A .res file starts with an empty entry that marks it as 32-bit.
    let mut out = Vec::new();
    push_resource(&mut out, 0, 0, 0, &[]);
    // GRPICONDIR: reserved, type 1 (icon), count; then 14-byte entries that
    // refer to the RT_ICON ids instead of file offsets.
    let mut group = Vec::new();
    group.extend(0u16.to_le_bytes());
    group.extend(1u16.to_le_bytes());
    group.extend((images.len() as u16).to_le_bytes());
    for (i, img) in images.iter().enumerate() {
        let id = i as u16 + 1;
        push_resource(&mut out, RT_ICON, id, 0x1010, img.data); // MOVEABLE | DISCARDABLE
        group.extend([img.width, img.height, img.colors, 0]);
        group.extend(img.planes.to_le_bytes());
        group.extend(img.bit_count.to_le_bytes());
        group.extend((img.data.len() as u32).to_le_bytes());
        group.extend(id.to_le_bytes());
    }
    push_resource(&mut out, RT_GROUP_ICON, GROUP_ID, 0x1030, &group); // + PURE
    Ok(out)
}
