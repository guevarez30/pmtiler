mod pmtiles;
mod raster;

use std::cmp::{max, min};
use std::env;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const HEADER_LEN: u64 = 127;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TileType {
    Png = 2,
    Jpeg = 3,
    Webp = 4,
    Avif = 5,
}

impl TileType {
    fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_ascii_lowercase().as_str() {
            "png" => Some(Self::Png),
            "jpg" | "jpeg" => Some(Self::Jpeg),
            "webp" => Some(Self::Webp),
            "avif" => Some(Self::Avif),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Webp => "webp",
            Self::Avif => "avif",
        }
    }
}

#[derive(Clone, Debug)]
struct TilePath {
    z: u8,
    x: u32,
    y: u32,
    tile_id: u64,
    path: PathBuf,
    byte_len: u64,
}

#[derive(Default)]
struct TileStats {
    min_zoom: u8,
    max_zoom: u8,
    seen_any: bool,
    min_lon: f64,
    min_lat: f64,
    max_lon: f64,
    max_lat: f64,
    min_tile_x: u32,
    min_tile_y: u32,
    max_tile_x: u32,
    max_tile_y: u32,
}

struct PackOptions {
    input_dir: PathBuf,
    output: PathBuf,
    name: Option<String>,
    description: Option<String>,
    attribution: Option<String>,
    tile_size: u32,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("pmtiler: {err}");
        std::process::exit(1);
    }
}

fn run() -> io::Result<()> {
    let mut args = env::args().skip(1);
    let Some(command) = args.next() else {
        print_help();
        return Ok(());
    };

    match command.as_str() {
        "pack" => {
            let opts = parse_pack_args(args.collect())?;
            pack_tile_directory(opts)
        }
        "inspect" => {
            let archive = parse_inspect_args(args.collect())?;
            inspect_pmtiles(&archive)
        }
        "raster" => raster::run(args.collect()),
        "help" => match args.next().as_deref() {
            Some("pack") => {
                print_pack_help();
                Ok(())
            }
            Some("raster") => {
                raster::print_raster_help();
                Ok(())
            }
            Some("inspect") => {
                print_inspect_help();
                Ok(())
            }
            Some(command) => Err(invalid_input(format!("unknown command `{command}`"))),
            None => {
                print_help();
                Ok(())
            }
        },
        "-h" | "--help" => {
            print_help();
            Ok(())
        }
        _ => Err(invalid_input(format!("unknown command `{command}`"))),
    }
}

fn print_help() {
    println!(
        "\
pmtiler

Build, inspect, and render PMTiles raster archives.

Usage:
  pmtiler <COMMAND>

Commands:
  pack      Pack an XYZ tile directory into PMTiles
  raster    Render a GDAL raster source into PMTiles
  inspect   Show PMTiles header and metadata
  help      Show this help

Command help:
  pmtiler help pack
  pmtiler help raster
  pmtiler help inspect

Examples:
  pmtiler pack [OPTIONS] <TILE_DIR> <OUTPUT.pmtiles>
  pmtiler raster <RASTER_OR_VRT> <OUTPUT.pmtiles> --zoom MIN-MAX [OPTIONS]
  pmtiler inspect <ARCHIVE.pmtiles>
"
    );
}

fn print_pack_help() {
    println!(
        "\
pmtiler pack

Pack an XYZ raster tile directory into a PMTiles v3 archive.

Usage:
  pmtiler pack [OPTIONS] <TILE_DIR> <OUTPUT.pmtiles>

XYZ tile layout:
  <TILE_DIR>/<z>/<x>/<y>.webp
  <TILE_DIR>/<z>/<x>/<y>.png
  <TILE_DIR>/<z>/<x>/<y>.jpg

Pack options:
  --name <text>          Metadata name
  --description <text>   Metadata description
  --attribution <text>   Metadata attribution
  --tile-size <px>       Tile size metadata value [default: 512]
  -h, --help             Show help
"
    );
}

fn print_inspect_help() {
    println!(
        "\
pmtiler inspect

Show PMTiles header, bounds, sections, and metadata.

Usage:
  pmtiler inspect <ARCHIVE.pmtiles>

Options:
  -h, --help             Show help
"
    );
}

fn parse_inspect_args(args: Vec<String>) -> io::Result<PathBuf> {
    if args.len() == 1 && matches!(args[0].as_str(), "-h" | "--help") {
        print_inspect_help();
        std::process::exit(0);
    }
    if args.len() != 1 {
        return Err(invalid_input("inspect requires <ARCHIVE.pmtiles>"));
    }
    Ok(PathBuf::from(&args[0]))
}

fn parse_pack_args(args: Vec<String>) -> io::Result<PackOptions> {
    let mut positional = Vec::new();
    let mut name = None;
    let mut description = None;
    let mut attribution = None;
    let mut tile_size = 512;
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                print_pack_help();
                std::process::exit(0);
            }
            "--name" => {
                i += 1;
                name = Some(expect_arg(&args, i, "--name")?.to_owned());
            }
            "--description" => {
                i += 1;
                description = Some(expect_arg(&args, i, "--description")?.to_owned());
            }
            "--attribution" => {
                i += 1;
                attribution = Some(expect_arg(&args, i, "--attribution")?.to_owned());
            }
            "--tile-size" => {
                i += 1;
                tile_size = expect_arg(&args, i, "--tile-size")?
                    .parse()
                    .map_err(|_| invalid_input("--tile-size must be an integer"))?;
            }
            arg if arg.starts_with('-') => {
                return Err(invalid_input(format!("unknown option `{arg}`")));
            }
            arg => positional.push(PathBuf::from(arg)),
        }

        i += 1;
    }

    if positional.len() != 2 {
        return Err(invalid_input(
            "pack requires <TILE_DIR> and <OUTPUT.pmtiles>",
        ));
    }

    Ok(PackOptions {
        input_dir: positional.remove(0),
        output: positional.remove(0),
        name,
        description,
        attribution,
        tile_size,
    })
}

fn expect_arg<'a>(args: &'a [String], index: usize, option: &str) -> io::Result<&'a str> {
    args.get(index)
        .map(String::as_str)
        .ok_or_else(|| invalid_input(format!("{option} requires a value")))
}

fn pack_tile_directory(opts: PackOptions) -> io::Result<()> {
    let mut tiles = Vec::new();
    let mut stats = TileStats::default();
    let mut tile_type = None;

    scan_tile_dir(&opts.input_dir, &mut |tile| {
        let ext = tile
            .path
            .extension()
            .and_then(OsStr::to_str)
            .and_then(TileType::from_extension)
            .expect("scanner only returns supported tile extensions");

        if let Some(existing) = tile_type {
            if existing != ext {
                return Err(invalid_input(
                    "mixed tile image formats are not supported yet",
                ));
            }
        } else {
            tile_type = Some(ext);
        }

        stats.observe(&tile);
        tiles.push(tile);
        Ok(())
    })?;

    if tiles.is_empty() {
        return Err(invalid_input("no XYZ image tiles found"));
    }

    tiles.sort_by_key(|tile| tile.tile_id);

    let tile_type = tile_type.expect("non-empty tile list has a type");
    let metadata = build_metadata(&opts, &stats, tile_type);
    eprintln!(
        "Packing {} tiles from {}",
        format_count(tiles.len() as u64),
        opts.input_dir.display()
    );
    write_pmtiles(&opts.output, &tiles, &stats, tile_type, metadata.as_bytes())?;
    print_pack_summary(&opts.output, &tiles, &stats, tile_type)
}

fn scan_tile_dir<F>(root: &Path, on_tile: &mut F) -> io::Result<()>
where
    F: FnMut(TilePath) -> io::Result<()>,
{
    for z_entry in fs::read_dir(root)? {
        let z_entry = z_entry?;
        if !z_entry.file_type()?.is_dir() {
            continue;
        }

        let Some(z) = parse_u8(&z_entry.file_name()) else {
            continue;
        };

        for x_entry in fs::read_dir(z_entry.path())? {
            let x_entry = x_entry?;
            if !x_entry.file_type()?.is_dir() {
                continue;
            }

            let Some(x) = parse_u32(&x_entry.file_name()) else {
                continue;
            };

            for y_entry in fs::read_dir(x_entry.path())? {
                let y_entry = y_entry?;
                if !y_entry.file_type()?.is_file() {
                    continue;
                }

                let path = y_entry.path();
                let Some(ext) = path.extension().and_then(OsStr::to_str) else {
                    continue;
                };
                if TileType::from_extension(ext).is_none() {
                    continue;
                }

                let Some(stem) = path.file_stem().and_then(OsStr::to_str) else {
                    continue;
                };
                let Ok(y) = stem.parse::<u32>() else {
                    continue;
                };

                validate_xyz(z, x, y)?;

                let byte_len = y_entry.metadata()?.len();
                if byte_len > u32::MAX as u64 {
                    return Err(invalid_input(format!(
                        "tile exceeds PMTiles entry length limit: {}",
                        path.display()
                    )));
                }

                on_tile(TilePath {
                    z,
                    x,
                    y,
                    tile_id: pmtiles::zxy_to_tile_id(z, x, y)?,
                    path,
                    byte_len,
                })?;
            }
        }
    }

    Ok(())
}

fn parse_u8(value: &OsStr) -> Option<u8> {
    value.to_str()?.parse().ok()
}

fn parse_u32(value: &OsStr) -> Option<u32> {
    value.to_str()?.parse().ok()
}

fn validate_xyz(z: u8, x: u32, y: u32) -> io::Result<()> {
    if z > 31 {
        return Err(invalid_input("PMTiles tile ids support zoom levels 0..31"));
    }

    let limit = 1_u32
        .checked_shl(z.into())
        .ok_or_else(|| invalid_input("invalid zoom level"))?;

    if x >= limit || y >= limit {
        return Err(invalid_input(format!(
            "tile {z}/{x}/{y} is outside XYZ bounds"
        )));
    }

    Ok(())
}

impl TileStats {
    fn observe(&mut self, tile: &TilePath) {
        if !self.seen_any {
            self.min_zoom = tile.z;
            self.max_zoom = tile.z;
            self.min_tile_x = tile.x;
            self.max_tile_x = tile.x;
            self.min_tile_y = tile.y;
            self.max_tile_y = tile.y;
            let (west, south, east, north) = tile_bounds_lonlat(tile.z, tile.x, tile.y);
            self.min_lon = west;
            self.min_lat = south;
            self.max_lon = east;
            self.max_lat = north;
            self.seen_any = true;
            return;
        }

        self.min_zoom = min(self.min_zoom, tile.z);
        self.max_zoom = max(self.max_zoom, tile.z);
        self.min_tile_x = min(self.min_tile_x, tile.x);
        self.max_tile_x = max(self.max_tile_x, tile.x);
        self.min_tile_y = min(self.min_tile_y, tile.y);
        self.max_tile_y = max(self.max_tile_y, tile.y);

        let (west, south, east, north) = tile_bounds_lonlat(tile.z, tile.x, tile.y);
        self.min_lon = self.min_lon.min(west);
        self.min_lat = self.min_lat.min(south);
        self.max_lon = self.max_lon.max(east);
        self.max_lat = self.max_lat.max(north);
    }
}

fn tile_bounds_lonlat(z: u8, x: u32, y: u32) -> (f64, f64, f64, f64) {
    let west = tile_x_to_lon(x, z);
    let east = tile_x_to_lon(x + 1, z);
    let north = tile_y_to_lat(y, z);
    let south = tile_y_to_lat(y + 1, z);
    (west, south, east, north)
}

fn tile_x_to_lon(x: u32, z: u8) -> f64 {
    f64::from(x) / f64::from(1_u32 << z) * 360.0 - 180.0
}

fn tile_y_to_lat(y: u32, z: u8) -> f64 {
    let n =
        std::f64::consts::PI - 2.0 * std::f64::consts::PI * f64::from(y) / f64::from(1_u32 << z);
    n.sinh().atan().to_degrees()
}

fn build_metadata(opts: &PackOptions, stats: &TileStats, tile_type: TileType) -> String {
    let name = opts
        .name
        .clone()
        .or_else(|| {
            opts.input_dir
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "pmtiler archive".to_owned());
    let description = opts.description.clone().unwrap_or_else(|| name.clone());
    let attribution = opts.attribution.clone().unwrap_or_default();

    format!(
        "{{\"name\":\"{}\",\"description\":\"{}\",\"attribution\":\"{}\",\"type\":\"overlay\",\"version\":\"1.0.0\",\"format\":\"{}\",\"tileSize\":{},\"minzoom\":{},\"maxzoom\":{},\"bounds\":[{:.7},{:.7},{:.7},{:.7}],\"center\":[{:.7},{:.7},{}]}}",
        json_escape(&name),
        json_escape(&description),
        json_escape(&attribution),
        tile_type.as_str(),
        opts.tile_size,
        stats.min_zoom,
        stats.max_zoom,
        stats.min_lon,
        stats.min_lat,
        stats.max_lon,
        stats.max_lat,
        (stats.min_lon + stats.max_lon) / 2.0,
        (stats.min_lat + stats.max_lat) / 2.0,
        stats.min_zoom
    )
}

fn json_escape(value: &str) -> String {
    let mut escaped = String::new();
    for ch in value.chars() {
        match ch {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            ch => escaped.push(ch),
        }
    }
    escaped
}

fn print_pack_summary(
    output: &Path,
    tiles: &[TilePath],
    stats: &TileStats,
    tile_type: TileType,
) -> io::Result<()> {
    let size = fs::metadata(output)?.len();

    println!("Created {}", output.display());
    print_field("Tiles", &format_count(tiles.len() as u64));
    print_field("Zooms", &format!("{}..{}", stats.min_zoom, stats.max_zoom));
    print_field("Format", tile_type.as_str());
    print_field(
        "Bounds",
        &format_bounds(stats.min_lon, stats.min_lat, stats.max_lon, stats.max_lat),
    );
    print_field("Size", &format_bytes(size));
    Ok(())
}

fn inspect_pmtiles(path: &Path) -> io::Result<()> {
    let mut file = File::open(path)?;
    let mut header_bytes = [0_u8; HEADER_LEN as usize];
    file.read_exact(&mut header_bytes)?;
    let header = ParsedHeader::parse(&header_bytes)?;

    println!("Archive");
    print_field("File", &path.display().to_string());
    print_field("Version", &header.version.to_string());
    print_field("Clustered", bool_label(header.clustered));

    println!();
    println!("Tiles");
    print_field("Type", tile_type_name(header.tile_type));
    print_field(
        "Tile compression",
        compression_name(header.tile_compression),
    );
    print_field(
        "Internal compression",
        compression_name(header.internal_compression),
    );
    print_field(
        "Zooms",
        &format!("{}..{}", header.min_zoom, header.max_zoom),
    );
    print_field("Addressed", &format_count(header.addressed_tiles_count));
    print_field("Entries", &format_count(header.tile_entries_count));
    print_field("Contents", &format_count(header.tile_contents_count));

    println!();
    println!("Bounds");
    print_field(
        "W/S/E/N",
        &format_bounds(
            from_e7(header.min_lon_e7),
            from_e7(header.min_lat_e7),
            from_e7(header.max_lon_e7),
            from_e7(header.max_lat_e7),
        ),
    );
    print_field(
        "Center",
        &format!(
            "{:.7}, {:.7}, z{}",
            from_e7(header.center_lon_e7),
            from_e7(header.center_lat_e7),
            header.center_zoom
        ),
    );

    println!();
    println!("Sections");
    print_section("Root", header.root_length, header.root_offset);
    print_section("Metadata", header.metadata_length, header.metadata_offset);
    print_section(
        "Leaves",
        header.leaf_directory_length,
        header.leaf_directory_offset,
    );
    print_section(
        "Tile data",
        header.tile_data_length,
        header.tile_data_offset,
    );

    if header.metadata_length > 0 {
        file.seek(SeekFrom::Start(header.metadata_offset))?;
        let mut metadata = vec![0_u8; header.metadata_length as usize];
        file.read_exact(&mut metadata)?;
        println!();
        println!("Metadata");
        if header.internal_compression == 1 {
            print_field("JSON", &String::from_utf8_lossy(&metadata));
        } else {
            print_field(
                "Payload",
                &format!(
                    "{} ({})",
                    format_bytes(metadata.len() as u64),
                    compression_name(header.internal_compression)
                ),
            );
        }
    }

    Ok(())
}

fn print_field(label: &str, value: &str) {
    println!("  {label:<20} {value}");
}

fn print_section(label: &str, length: u64, offset: u64) {
    print_field(
        label,
        &format!("{} @ {}", format_bytes(length), format_bytes(offset)),
    );
}

fn format_bounds(west: f64, south: f64, east: f64, north: f64) -> String {
    format!("{west:.7}, {south:.7}, {east:.7}, {north:.7}")
}

fn bool_label(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn format_count(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

struct ParsedHeader {
    version: u8,
    root_offset: u64,
    root_length: u64,
    metadata_offset: u64,
    metadata_length: u64,
    leaf_directory_offset: u64,
    leaf_directory_length: u64,
    tile_data_offset: u64,
    tile_data_length: u64,
    addressed_tiles_count: u64,
    tile_entries_count: u64,
    tile_contents_count: u64,
    clustered: bool,
    internal_compression: u8,
    tile_compression: u8,
    tile_type: u8,
    min_zoom: u8,
    max_zoom: u8,
    min_lon_e7: i32,
    min_lat_e7: i32,
    max_lon_e7: i32,
    max_lat_e7: i32,
    center_zoom: u8,
    center_lon_e7: i32,
    center_lat_e7: i32,
}

impl ParsedHeader {
    fn parse(buf: &[u8; HEADER_LEN as usize]) -> io::Result<Self> {
        if &buf[0..7] != b"PMTiles" {
            return Err(invalid_input("not a PMTiles archive"));
        }
        if buf[7] != 3 {
            return Err(invalid_input(format!(
                "unsupported PMTiles version {}",
                buf[7]
            )));
        }

        Ok(Self {
            version: buf[7],
            root_offset: read_u64(buf, 8),
            root_length: read_u64(buf, 16),
            metadata_offset: read_u64(buf, 24),
            metadata_length: read_u64(buf, 32),
            leaf_directory_offset: read_u64(buf, 40),
            leaf_directory_length: read_u64(buf, 48),
            tile_data_offset: read_u64(buf, 56),
            tile_data_length: read_u64(buf, 64),
            addressed_tiles_count: read_u64(buf, 72),
            tile_entries_count: read_u64(buf, 80),
            tile_contents_count: read_u64(buf, 88),
            clustered: buf[96] == 1,
            internal_compression: buf[97],
            tile_compression: buf[98],
            tile_type: buf[99],
            min_zoom: buf[100],
            max_zoom: buf[101],
            min_lon_e7: read_i32(buf, 102),
            min_lat_e7: read_i32(buf, 106),
            max_lon_e7: read_i32(buf, 110),
            max_lat_e7: read_i32(buf, 114),
            center_zoom: buf[118],
            center_lon_e7: read_i32(buf, 119),
            center_lat_e7: read_i32(buf, 123),
        })
    }
}

fn read_u64(buf: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(buf[offset..offset + 8].try_into().expect("valid u64 slice"))
}

fn read_i32(buf: &[u8], offset: usize) -> i32 {
    i32::from_le_bytes(buf[offset..offset + 4].try_into().expect("valid i32 slice"))
}

fn tile_type_name(value: u8) -> &'static str {
    match value {
        1 => "mvt",
        2 => "png",
        3 => "jpeg",
        4 => "webp",
        5 => "avif",
        6 => "mlt",
        _ => "unknown",
    }
}

fn compression_name(value: u8) -> &'static str {
    match value {
        1 => "none",
        2 => "gzip",
        3 => "brotli",
        4 => "zstd",
        _ => "unknown",
    }
}

fn write_pmtiles(
    output: &Path,
    tiles: &[TilePath],
    stats: &TileStats,
    tile_type: TileType,
    metadata: &[u8],
) -> io::Result<()> {
    let mut writer = pmtiles::ArchiveWriter::create(output)?;

    for tile in tiles {
        writer.add_tile(tile.tile_id, tile.byte_len as u32);
    }

    let layout = writer.finish_directories(metadata)?;

    let mut copy_buffer = vec![0_u8; 1024 * 1024];
    let started = std::time::Instant::now();
    for (index, tile) in tiles.iter().enumerate() {
        let mut input = File::open(&tile.path)?;
        loop {
            let read = input.read(&mut copy_buffer)?;
            if read == 0 {
                break;
            }
            writer.write_tile_data(&copy_buffer[..read])?;
        }

        let completed = index + 1;
        if completed == tiles.len() || completed % 100 == 0 {
            eprintln!(
                "Progress: {}/{} tiles ({:.0}%) elapsed {:.1}s",
                format_count(completed as u64),
                format_count(tiles.len() as u64),
                completed as f64 / tiles.len() as f64 * 100.0,
                started.elapsed().as_secs_f64()
            );
        }
    }

    let header = pmtiles::Header {
        root_offset: HEADER_LEN,
        root_length: layout.root_length,
        metadata_offset: layout.metadata_offset,
        metadata_length: layout.metadata_length,
        leaf_directory_offset: layout.leaf_directory_offset,
        leaf_directory_length: layout.leaf_directory_length,
        tile_data_offset: layout.tile_data_offset,
        tile_data_length: layout.tile_data_length,
        addressed_tiles_count: layout.entries_count,
        tile_entries_count: layout.entries_count,
        tile_contents_count: layout.entries_count,
        clustered: true,
        internal_compression: 1,
        tile_compression: 1,
        tile_type: tile_type as u8,
        min_zoom: stats.min_zoom,
        max_zoom: stats.max_zoom,
        min_lon_e7: to_e7(stats.min_lon),
        min_lat_e7: to_e7(stats.min_lat),
        max_lon_e7: to_e7(stats.max_lon),
        max_lat_e7: to_e7(stats.max_lat),
        center_zoom: stats.min_zoom,
        center_lon_e7: to_e7((stats.min_lon + stats.max_lon) / 2.0),
        center_lat_e7: to_e7((stats.min_lat + stats.max_lat) / 2.0),
    };
    writer.write_header(&header)
}

fn to_e7(value: f64) -> i32 {
    (value * 10_000_000.0).round() as i32
}

fn from_e7(value: i32) -> f64 {
    f64::from(value) / 10_000_000.0
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}
