pub fn firmware() -> Vec<u8> {
    let mut image = vec![0; 4112];
    image[..6].copy_from_slice(b"AIC.FW");
    image[8..12].copy_from_slice(b"D213");
    image[72..79].copy_from_slice(b"Offline");
    image[136..141].copy_from_slice(b"1.0.0");
    image[200..208].copy_from_slice(b"spi-nand");
    for (position, value) in [(332, 2048u32), (336, 2048), (340, 4096), (344, 16)] {
        image[position..position + 4].copy_from_slice(&value.to_le_bytes());
    }
    for (index, (name, part, media, offset, length, crc)) in [
        ("image.updater.ddr", "", "ram", 4096u32, 4u32, 0x86451ae2u32),
        ("image.info", "", "ram", 0, 2048, 0xe9bc1b28),
        ("image.target.spl", "spl", "spi-nand", 4100, 5, 0xcbea3cbd),
        ("image.target.os", "os", "spi-nand", 4105, 7, 0xa56762bb),
    ]
    .into_iter()
    .enumerate()
    {
        let start = 2048 + index * 512;
        let meta = &mut image[start..start + 512];
        meta[..4].copy_from_slice(b"META");
        meta[8..8 + name.len()].copy_from_slice(name.as_bytes());
        meta[72..72 + part.len()].copy_from_slice(part.as_bytes());
        meta[112..112 + media.len()].copy_from_slice(media.as_bytes());
        meta[136..140].copy_from_slice(&offset.to_le_bytes());
        meta[140..144].copy_from_slice(&length.to_le_bytes());
        meta[144..148].copy_from_slice(&crc.to_le_bytes());
    }
    image[4096..].copy_from_slice(b"DDR!SPL!!OS DEMO");
    image
}
