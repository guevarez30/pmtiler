use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

pub const HEADER_LEN: u64 = 127;
pub const TARGET_ROOT_LEN: usize = 16_384 - HEADER_LEN as usize;

#[derive(Clone, Debug)]
pub struct Entry {
    pub tile_id: u64,
    pub offset: u64,
    pub length: u32,
    pub run_length: u32,
}

#[derive(Clone, Debug)]
pub struct Header {
    pub root_offset: u64,
    pub root_length: u64,
    pub metadata_offset: u64,
    pub metadata_length: u64,
    pub leaf_directory_offset: u64,
    pub leaf_directory_length: u64,
    pub tile_data_offset: u64,
    pub tile_data_length: u64,
    pub addressed_tiles_count: u64,
    pub tile_entries_count: u64,
    pub tile_contents_count: u64,
    pub clustered: bool,
    pub internal_compression: u8,
    pub tile_compression: u8,
    pub tile_type: u8,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub min_lon_e7: i32,
    pub min_lat_e7: i32,
    pub max_lon_e7: i32,
    pub max_lat_e7: i32,
    pub center_zoom: u8,
    pub center_lon_e7: i32,
    pub center_lat_e7: i32,
}

impl Header {
    pub fn serialize(&self) -> [u8; HEADER_LEN as usize] {
        let mut out = [0_u8; HEADER_LEN as usize];
        out[0..7].copy_from_slice(b"PMTiles");
        out[7] = 3;
        write_u64(&mut out, 8, self.root_offset);
        write_u64(&mut out, 16, self.root_length);
        write_u64(&mut out, 24, self.metadata_offset);
        write_u64(&mut out, 32, self.metadata_length);
        write_u64(&mut out, 40, self.leaf_directory_offset);
        write_u64(&mut out, 48, self.leaf_directory_length);
        write_u64(&mut out, 56, self.tile_data_offset);
        write_u64(&mut out, 64, self.tile_data_length);
        write_u64(&mut out, 72, self.addressed_tiles_count);
        write_u64(&mut out, 80, self.tile_entries_count);
        write_u64(&mut out, 88, self.tile_contents_count);
        out[96] = u8::from(self.clustered);
        out[97] = self.internal_compression;
        out[98] = self.tile_compression;
        out[99] = self.tile_type;
        out[100] = self.min_zoom;
        out[101] = self.max_zoom;
        write_i32(&mut out, 102, self.min_lon_e7);
        write_i32(&mut out, 106, self.min_lat_e7);
        write_i32(&mut out, 110, self.max_lon_e7);
        write_i32(&mut out, 114, self.max_lat_e7);
        out[118] = self.center_zoom;
        write_i32(&mut out, 119, self.center_lon_e7);
        write_i32(&mut out, 123, self.center_lat_e7);
        out
    }
}

pub struct ArchiveWriter {
    file: BufWriter<File>,
    entries: Vec<Entry>,
    tile_data_length: u64,
}

impl ArchiveWriter {
    pub fn create(output: &Path) -> io::Result<Self> {
        let mut file = BufWriter::new(File::create(output)?);
        file.write_all(&[0_u8; HEADER_LEN as usize])?;
        Ok(Self {
            file,
            entries: Vec::new(),
            tile_data_length: 0,
        })
    }

    pub fn add_tile(&mut self, tile_id: u64, length: u32) {
        self.entries.push(Entry {
            tile_id,
            offset: self.tile_data_length,
            length,
            run_length: 1,
        });
        self.tile_data_length += u64::from(length);
    }

    pub fn finish_directories(&mut self, metadata: &[u8]) -> io::Result<ArchiveLayout> {
        self.entries.sort_by_key(|entry| entry.tile_id);
        let (root, leaves) = optimize_directories(&self.entries, TARGET_ROOT_LEN);

        let metadata_offset = HEADER_LEN + root.len() as u64;
        let leaf_directory_offset = metadata_offset + metadata.len() as u64;
        let tile_data_offset = leaf_directory_offset + leaves.len() as u64;

        self.file.write_all(&root)?;
        self.file.write_all(metadata)?;
        self.file.write_all(&leaves)?;

        Ok(ArchiveLayout {
            root_length: root.len() as u64,
            metadata_offset,
            metadata_length: metadata.len() as u64,
            leaf_directory_offset,
            leaf_directory_length: leaves.len() as u64,
            tile_data_offset,
            tile_data_length: self.tile_data_length,
            entries_count: self.entries.len() as u64,
        })
    }

    pub fn write_tile_data(&mut self, data: &[u8]) -> io::Result<()> {
        self.file.write_all(data)
    }

    pub fn write_header(mut self, header: &Header) -> io::Result<()> {
        self.file.flush()?;
        let mut file = self.file.into_inner()?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&header.serialize())
    }
}

pub struct ArchiveLayout {
    pub root_length: u64,
    pub metadata_offset: u64,
    pub metadata_length: u64,
    pub leaf_directory_offset: u64,
    pub leaf_directory_length: u64,
    pub tile_data_offset: u64,
    pub tile_data_length: u64,
    pub entries_count: u64,
}

pub fn optimize_directories(entries: &[Entry], target_root_len: usize) -> (Vec<u8>, Vec<u8>) {
    let root = serialize_directory(entries);
    if root.len() <= target_root_len {
        return (root, Vec::new());
    }

    let mut leaf_size = 4096;
    loop {
        let (root, leaves) = build_root_and_leaves(entries, leaf_size);
        if root.len() <= target_root_len {
            return (root, leaves);
        }
        leaf_size *= 2;
    }
}

fn build_root_and_leaves(entries: &[Entry], leaf_size: usize) -> (Vec<u8>, Vec<u8>) {
    let mut root_entries = Vec::new();
    let mut leaves = Vec::new();

    for chunk in entries.chunks(leaf_size) {
        let leaf = serialize_directory(chunk);
        root_entries.push(Entry {
            tile_id: chunk[0].tile_id,
            offset: leaves.len() as u64,
            length: leaf.len() as u32,
            run_length: 0,
        });
        leaves.extend_from_slice(&leaf);
    }

    (serialize_directory(&root_entries), leaves)
}

pub fn serialize_directory(entries: &[Entry]) -> Vec<u8> {
    let mut out = Vec::new();
    write_varint(entries.len() as u64, &mut out);

    let mut last_id = 0;
    for entry in entries {
        write_varint(entry.tile_id - last_id, &mut out);
        last_id = entry.tile_id;
    }

    for entry in entries {
        write_varint(entry.run_length.into(), &mut out);
    }

    for entry in entries {
        write_varint(entry.length.into(), &mut out);
    }

    for (index, entry) in entries.iter().enumerate() {
        if index > 0
            && entry.offset == entries[index - 1].offset + u64::from(entries[index - 1].length)
        {
            write_varint(0, &mut out);
        } else {
            write_varint(entry.offset + 1, &mut out);
        }
    }

    out
}

pub fn write_varint(mut value: u64, out: &mut Vec<u8>) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    }
}

pub fn zxy_to_tile_id(z: u8, x: u32, y: u32) -> io::Result<u64> {
    if z > 31 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "PMTiles tile ids support zoom levels 0..31",
        ));
    }

    let mut x = i64::from(x);
    let mut y = i64::from(y);
    let mut acc = ((1_u64 << (u64::from(z) * 2)) - 1) / 3;
    let mut a = i32::from(z) - 1;
    while a >= 0 {
        let s = 1_i64 << a;
        let rx = x & s;
        let ry = y & s;
        acc += (((3 * rx) ^ ry) as u64) << a;
        let rotated = rotate(s, x, y, rx, ry);
        x = rotated.0;
        y = rotated.1;
        a -= 1;
    }

    Ok(acc)
}

fn rotate(n: i64, mut x: i64, mut y: i64, rx: i64, ry: i64) -> (i64, i64) {
    if ry == 0 {
        if rx != 0 {
            x = n - 1 - x;
            y = n - 1 - y;
        }
        std::mem::swap(&mut x, &mut y);
    }
    (x, y)
}

fn write_u64(out: &mut [u8], offset: usize, value: u64) {
    out[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn write_i32(out: &mut [u8], offset: usize, value: i32) {
    out[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zxy_tile_ids_match_pmtiles_order() {
        assert_eq!(zxy_to_tile_id(0, 0, 0).unwrap(), 0);
        assert_eq!(zxy_to_tile_id(1, 0, 0).unwrap(), 1);
        assert_eq!(zxy_to_tile_id(1, 0, 1).unwrap(), 2);
        assert_eq!(zxy_to_tile_id(1, 1, 1).unwrap(), 3);
        assert_eq!(zxy_to_tile_id(1, 1, 0).unwrap(), 4);
    }

    #[test]
    fn directory_serialization_delta_encodes_offsets() {
        let entries = vec![
            Entry {
                tile_id: 1,
                offset: 0,
                length: 10,
                run_length: 1,
            },
            Entry {
                tile_id: 2,
                offset: 10,
                length: 20,
                run_length: 1,
            },
        ];

        assert_eq!(
            serialize_directory(&entries),
            vec![2, 1, 1, 1, 1, 10, 20, 1, 0]
        );
    }
}
