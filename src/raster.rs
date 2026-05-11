use std::ffi::CStr;
use std::io;
use std::path::{Path, PathBuf};

use crate::pmtiles;
use gdal::GeoTransformEx;
use gdal::cpl::CslStringList;
use gdal::spatial_ref::{AxisMappingStrategy, CoordTransform, SpatialRef};
use std::collections::BTreeMap;

const WEBMERC_MAX: f64 = 20_037_508.342_789_244;
const WEBMERC_SIZE: f64 = WEBMERC_MAX * 2.0;
const DEFAULT_CHUNK_TILES: u32 = 8;
const MIN_RESERVED_DIRECTORY_BYTES: u64 = 64 * 1024;
const MAX_RESERVED_DIRECTORY_BYTES: u64 = 16 * 1024 * 1024;

#[link(name = "webp")]
unsafe extern "C" {
    fn WebPEncodeRGB(
        rgb: *const u8,
        width: i32,
        height: i32,
        stride: i32,
        quality_factor: f32,
        output: *mut *mut u8,
    ) -> usize;
    fn WebPEncodeRGBA(
        rgba: *const u8,
        width: i32,
        height: i32,
        stride: i32,
        quality_factor: f32,
        output: *mut *mut u8,
    ) -> usize;
    fn WebPFree(ptr: *mut std::ffi::c_void);
}

#[derive(Clone, Debug)]
pub struct RasterOptions {
    pub input: PathBuf,
    pub output: PathBuf,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub bounds: [f64; 4],
    pub format: RasterFormat,
    pub tile_size: u32,
    pub workers: usize,
    pub chunk_tiles: Option<u32>,
    pub warp_memory_bytes: f64,
    pub warp_threads: WarpThreads,
    pub resampling: Resampling,
    pub strategy: StrategyPreference,
    pub warp_options: Vec<(String, String)>,
    pub plan_only: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RasterFormat {
    Png,
    Jpeg,
    Webp,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WarpThreads {
    All,
    Count(usize),
}

impl WarpThreads {
    fn as_gdal_value(self) -> String {
        match self {
            Self::All => "ALL_CPUS".to_owned(),
            Self::Count(count) => count.to_string(),
        }
    }

    fn label(self) -> String {
        match self {
            Self::All => "all".to_owned(),
            Self::Count(count) => count.to_string(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Resampling {
    Nearest,
    Bilinear,
    Cubic,
    CubicSpline,
    Lanczos,
    Average,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StrategyPreference {
    Auto,
    SameCrs,
    Geographic,
    GdalWarp,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RenderStrategy {
    SameCrsWebMercator,
    GeographicWgs84,
    GdalWarp,
}

impl RenderStrategy {
    fn label(self) -> &'static str {
        match self {
            Self::SameCrsWebMercator => "Web Mercator fast path",
            Self::GeographicWgs84 => "EPSG:4326 fast path",
            Self::GdalWarp => "GDAL warp",
        }
    }
}

impl Resampling {
    fn label(self) -> &'static str {
        match self {
            Self::Nearest => "nearest",
            Self::Bilinear => "bilinear",
            Self::Cubic => "cubic",
            Self::CubicSpline => "cubicspline",
            Self::Lanczos => "lanczos",
            Self::Average => "average",
        }
    }

    fn as_gdal(self) -> gdal_sys::GDALResampleAlg::Type {
        match self {
            Self::Nearest => gdal_sys::GDALResampleAlg::GRA_NearestNeighbour,
            Self::Bilinear => gdal_sys::GDALResampleAlg::GRA_Bilinear,
            Self::Cubic => gdal_sys::GDALResampleAlg::GRA_Cubic,
            Self::CubicSpline => gdal_sys::GDALResampleAlg::GRA_CubicSpline,
            Self::Lanczos => gdal_sys::GDALResampleAlg::GRA_Lanczos,
            Self::Average => gdal_sys::GDALResampleAlg::GRA_Average,
        }
    }
}

impl RasterFormat {
    fn as_str(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Webp => "webp",
        }
    }

    fn pmtiles_type(self) -> u8 {
        match self {
            Self::Png => 2,
            Self::Jpeg => 3,
            Self::Webp => 4,
        }
    }
}

impl StrategyPreference {
    fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::SameCrs => "same-crs",
            Self::Geographic => "geographic",
            Self::GdalWarp => "gdal-warp",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TileJob {
    pub z: u8,
    pub x: u32,
    pub y: u32,
    pub tile_id: u64,
}

pub fn run(args: Vec<String>) -> io::Result<()> {
    let opts = parse_args(args)?;
    let jobs = enumerate_tile_jobs(opts.bounds, opts.min_zoom, opts.max_zoom)?;

    if opts.plan_only {
        print_plan(&opts, &jobs);
        return Ok(());
    }

    render_native(&opts, &jobs)
}

fn parse_args(args: Vec<String>) -> io::Result<RasterOptions> {
    let mut positional = Vec::new();
    let mut zoom = None;
    let mut bounds = None;
    let mut format = RasterFormat::Webp;
    let mut tile_size = 512;
    let mut workers = available_workers();
    let mut chunk_tiles = Some(DEFAULT_CHUNK_TILES);
    let mut warp_memory_bytes = 512.0 * 1024.0 * 1024.0;
    let mut warp_threads = WarpThreads::All;
    let mut resampling = Resampling::Bilinear;
    let mut strategy = StrategyPreference::Auto;
    let mut warp_options = Vec::new();
    let mut plan_only = false;
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                print_raster_help();
                std::process::exit(0);
            }
            "--plan" => plan_only = true,
            "--zoom" => {
                i += 1;
                zoom = Some(parse_zoom(expect_arg(&args, i, "--zoom")?)?);
            }
            arg if arg.starts_with("--zoom=") => {
                zoom = Some(parse_zoom(&arg["--zoom=".len()..])?);
            }
            "--bounds" => {
                i += 1;
                bounds = Some(parse_bounds(expect_arg(&args, i, "--bounds")?)?);
            }
            arg if arg.starts_with("--bounds=") => {
                bounds = Some(parse_bounds(&arg["--bounds=".len()..])?);
            }
            "--format" => {
                i += 1;
                format = parse_format(expect_arg(&args, i, "--format")?)?;
            }
            arg if arg.starts_with("--format=") => {
                format = parse_format(&arg["--format=".len()..])?;
            }
            "--tile-size" => {
                i += 1;
                tile_size = parse_tile_size(expect_arg(&args, i, "--tile-size")?)?;
            }
            arg if arg.starts_with("--tile-size=") => {
                tile_size = parse_tile_size(&arg["--tile-size=".len()..])?;
            }
            "--workers" => {
                i += 1;
                workers = parse_workers(expect_arg(&args, i, "--workers")?)?;
            }
            arg if arg.starts_with("--workers=") => {
                workers = parse_workers(&arg["--workers=".len()..])?;
            }
            "--chunk-tiles" => {
                i += 1;
                chunk_tiles = parse_chunk_tiles(expect_arg(&args, i, "--chunk-tiles")?)?;
            }
            arg if arg.starts_with("--chunk-tiles=") => {
                chunk_tiles = parse_chunk_tiles(&arg["--chunk-tiles=".len()..])?;
            }
            "--warp-memory" => {
                i += 1;
                warp_memory_bytes = parse_memory_bytes(expect_arg(&args, i, "--warp-memory")?)?;
            }
            arg if arg.starts_with("--warp-memory=") => {
                warp_memory_bytes = parse_memory_bytes(&arg["--warp-memory=".len()..])?;
            }
            "--warp-threads" => {
                i += 1;
                warp_threads = parse_warp_threads(expect_arg(&args, i, "--warp-threads")?)?;
            }
            arg if arg.starts_with("--warp-threads=") => {
                warp_threads = parse_warp_threads(&arg["--warp-threads=".len()..])?;
            }
            "--resampling" => {
                i += 1;
                resampling = parse_resampling(expect_arg(&args, i, "--resampling")?)?;
            }
            arg if arg.starts_with("--resampling=") => {
                resampling = parse_resampling(&arg["--resampling=".len()..])?;
            }
            "--strategy" => {
                i += 1;
                strategy = parse_strategy(expect_arg(&args, i, "--strategy")?)?;
            }
            arg if arg.starts_with("--strategy=") => {
                strategy = parse_strategy(&arg["--strategy=".len()..])?;
            }
            "--warp-option" => {
                i += 1;
                warp_options.push(parse_name_value(expect_arg(&args, i, "--warp-option")?)?);
            }
            arg if arg.starts_with("--warp-option=") => {
                warp_options.push(parse_name_value(&arg["--warp-option=".len()..])?);
            }
            arg if arg.starts_with('-') => {
                return Err(invalid_input(format!("unknown raster option `{arg}`")));
            }
            arg => positional.push(PathBuf::from(arg)),
        }
        i += 1;
    }

    if positional.len() != 2 {
        return Err(invalid_input(
            "raster requires <RASTER_OR_VRT> and <OUTPUT.pmtiles>",
        ));
    }

    let Some((min_zoom, max_zoom)) = zoom else {
        return Err(invalid_input("raster requires --zoom"));
    };
    let input = positional.remove(0);
    let output = positional.remove(0);
    let bounds = match bounds {
        Some(bounds) => bounds,
        None => infer_raster_bounds(&input)?,
    };

    Ok(RasterOptions {
        input,
        output,
        min_zoom,
        max_zoom,
        bounds,
        format,
        tile_size,
        workers,
        chunk_tiles,
        warp_memory_bytes,
        warp_threads,
        resampling,
        strategy,
        warp_options,
        plan_only,
    })
}

pub fn enumerate_tile_jobs(
    bounds: [f64; 4],
    min_zoom: u8,
    max_zoom: u8,
) -> io::Result<Vec<TileJob>> {
    if min_zoom > max_zoom {
        return Err(invalid_input("--zoom minimum must be <= maximum"));
    }
    if max_zoom > 31 {
        return Err(invalid_input("PMTiles tile ids support zoom levels 0..31"));
    }

    let [west, south, east, north] = bounds;
    let mut jobs = Vec::new();
    for z in min_zoom..=max_zoom {
        let (min_x, max_y) = lonlat_to_tile(west, south, z);
        let (max_x, min_y) = lonlat_to_tile(east, north, z);
        for y in min_y..=max_y {
            for x in min_x..=max_x {
                jobs.push(TileJob {
                    z,
                    x,
                    y,
                    tile_id: pmtiles::zxy_to_tile_id(z, x, y)?,
                });
            }
        }
    }
    jobs.sort_by_key(|job| job.tile_id);
    Ok(jobs)
}

pub fn tile_bounds_mercator(z: u8, x: u32, y: u32) -> (f64, f64, f64, f64) {
    let n = 1_u32 << z;
    let tile_span = WEBMERC_SIZE / f64::from(n);
    let minx = -WEBMERC_MAX + f64::from(x) * tile_span;
    let maxx = minx + tile_span;
    let maxy = WEBMERC_MAX - f64::from(y) * tile_span;
    let miny = maxy - tile_span;
    (minx, miny, maxx, maxy)
}

fn lonlat_to_tile(lon: f64, lat: f64, z: u8) -> (u32, u32) {
    let n = 1_u32 << z;
    let max_index = f64::from(n - 1);
    let lon = lon.clamp(-180.0, 180.0);
    let lat = lat.clamp(-85.051_128_78, 85.051_128_78);
    let x = ((lon + 180.0) / 360.0 * f64::from(n))
        .floor()
        .clamp(0.0, max_index);
    let lat_rad = lat.to_radians();
    let y = ((1.0 - lat_rad.tan().asinh() / std::f64::consts::PI) / 2.0 * f64::from(n))
        .floor()
        .clamp(0.0, max_index);
    (x as u32, y as u32)
}

fn parse_zoom(value: &str) -> io::Result<(u8, u8)> {
    if let Some((left, right)) = value.split_once('-') {
        return Ok((parse_zoom_part(left)?, parse_zoom_part(right)?));
    }
    let zoom = parse_zoom_part(value)?;
    Ok((zoom, zoom))
}

fn parse_zoom_part(value: &str) -> io::Result<u8> {
    value
        .parse()
        .map_err(|_| invalid_input("--zoom must be a zoom or range like 0-13"))
}

fn parse_bounds(value: &str) -> io::Result<[f64; 4]> {
    let parts = value
        .split(',')
        .map(str::parse::<f64>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| invalid_input("--bounds must be west,south,east,north"))?;
    if parts.len() != 4 {
        return Err(invalid_input("--bounds must be west,south,east,north"));
    }
    normalize_lonlat_bounds([parts[0], parts[1], parts[2], parts[3]], "--bounds")
}

fn infer_raster_bounds(path: &Path) -> io::Result<[f64; 4]> {
    let dataset = gdal::Dataset::open(path).map_err(gdal_to_io)?;
    let transform = dataset.geo_transform().map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "raster has no usable geotransform; pass --bounds west,south,east,north ({err})"
            ),
        )
    })?;
    let mut source_srs = dataset.spatial_ref().map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("raster has no usable CRS; pass --bounds west,south,east,north ({err})"),
        )
    })?;
    source_srs.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);

    let mut target_srs = SpatialRef::from_epsg(4326).map_err(gdal_to_io)?;
    target_srs.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);
    let coord_transform = CoordTransform::new(&source_srs, &target_srs).map_err(gdal_to_io)?;

    let width = unsafe { gdal_sys::GDALGetRasterXSize(dataset.c_dataset()) } as usize;
    let height = unsafe { gdal_sys::GDALGetRasterYSize(dataset.c_dataset()) } as usize;
    if width == 0 || height == 0 {
        return Err(invalid_input("raster has no pixels"));
    }

    let mut xs = Vec::new();
    let mut ys = Vec::new();
    let densify = 21_usize;
    for i in 0..=densify {
        let x = width as f64 * i as f64 / densify as f64;
        push_georef_point(&transform, x, 0.0, &mut xs, &mut ys);
        push_georef_point(&transform, x, height as f64, &mut xs, &mut ys);
    }
    for i in 1..densify {
        let y = height as f64 * i as f64 / densify as f64;
        push_georef_point(&transform, 0.0, y, &mut xs, &mut ys);
        push_georef_point(&transform, width as f64, y, &mut xs, &mut ys);
    }

    coord_transform
        .transform_coords(&mut xs, &mut ys, &mut [])
        .map_err(gdal_to_io)?;

    let mut west = f64::INFINITY;
    let mut south = f64::INFINITY;
    let mut east = f64::NEG_INFINITY;
    let mut north = f64::NEG_INFINITY;
    for (&lon, &lat) in xs.iter().zip(ys.iter()) {
        if lon.is_finite() && lat.is_finite() {
            west = west.min(lon);
            south = south.min(lat);
            east = east.max(lon);
            north = north.max(lat);
        }
    }

    if !west.is_finite() || !south.is_finite() || !east.is_finite() || !north.is_finite() {
        return Err(invalid_input(
            "could not infer finite raster bounds; pass --bounds west,south,east,north",
        ));
    }

    let bounds = normalize_lonlat_bounds([west, south, east, north], "inferred bounds")?;
    status_field(
        "Bounds inferred",
        &format!(
            "{:.7}, {:.7}, {:.7}, {:.7}",
            bounds[0], bounds[1], bounds[2], bounds[3]
        ),
    );
    Ok(bounds)
}

fn push_georef_point(
    transform: &[f64; 6],
    pixel: f64,
    line: f64,
    xs: &mut Vec<f64>,
    ys: &mut Vec<f64>,
) {
    let (x, y) = transform.apply(pixel, line);
    xs.push(x);
    ys.push(y);
}

fn normalize_lonlat_bounds(bounds: [f64; 4], label: &str) -> io::Result<[f64; 4]> {
    let [west, south, east, north] = bounds;
    if west >= east || south >= north {
        return Err(invalid_input(format!(
            "{label} must have west < east and south < north"
        )));
    }

    let clamped = [
        west.clamp(-180.0, 180.0),
        south.clamp(-85.051_128_78, 85.051_128_78),
        east.clamp(-180.0, 180.0),
        north.clamp(-85.051_128_78, 85.051_128_78),
    ];
    if clamped[0] >= clamped[2] || clamped[1] >= clamped[3] {
        return Err(invalid_input(format!(
            "{label} do not intersect the Web Mercator tileable extent; pass --bounds west,south,east,north"
        )));
    }
    Ok(clamped)
}

fn parse_format(value: &str) -> io::Result<RasterFormat> {
    match value {
        "png" => Ok(RasterFormat::Png),
        "jpeg" | "jpg" => Ok(RasterFormat::Jpeg),
        "webp" => Ok(RasterFormat::Webp),
        _ => Err(invalid_input("--format must be png, jpeg, jpg, or webp")),
    }
}

fn parse_tile_size(value: &str) -> io::Result<u32> {
    let tile_size = value
        .parse()
        .map_err(|_| invalid_input("--tile-size must be an integer"))?;
    if tile_size == 0 {
        return Err(invalid_input("--tile-size must be greater than zero"));
    }
    Ok(tile_size)
}

fn parse_workers(value: &str) -> io::Result<usize> {
    let workers = value
        .parse()
        .map_err(|_| invalid_input("--workers must be an integer"))?;
    if workers == 0 {
        return Err(invalid_input("--workers must be greater than zero"));
    }
    Ok(workers)
}

fn parse_chunk_tiles(value: &str) -> io::Result<Option<u32>> {
    if matches!(value, "disabled" | "disable" | "none" | "off") {
        return Ok(None);
    }
    let chunk_tiles = value
        .parse()
        .map_err(|_| invalid_input("--chunk-tiles must be an integer or disabled"))?;
    if chunk_tiles == 0 {
        return Err(invalid_input(
            "--chunk-tiles must be greater than zero or disabled",
        ));
    }
    Ok(Some(chunk_tiles))
}

fn parse_memory_bytes(value: &str) -> io::Result<f64> {
    let value = value.trim();
    if value.is_empty() {
        return Err(invalid_input("--warp-memory requires a value"));
    }

    let (number, multiplier) = match value.as_bytes().last().copied() {
        Some(b'k' | b'K') => (&value[..value.len() - 1], 1024.0),
        Some(b'm' | b'M') => (&value[..value.len() - 1], 1024.0 * 1024.0),
        Some(b'g' | b'G') => (&value[..value.len() - 1], 1024.0 * 1024.0 * 1024.0),
        _ => (value, 1024.0 * 1024.0),
    };
    let amount: f64 = number
        .parse()
        .map_err(|_| invalid_input("--warp-memory must be a number with optional K, M, or G"))?;
    if amount <= 0.0 {
        return Err(invalid_input("--warp-memory must be greater than zero"));
    }
    Ok(amount * multiplier)
}

fn parse_warp_threads(value: &str) -> io::Result<WarpThreads> {
    if value.eq_ignore_ascii_case("all") || value.eq_ignore_ascii_case("all_cpus") {
        return Ok(WarpThreads::All);
    }
    let count = value
        .parse()
        .map_err(|_| invalid_input("--warp-threads must be all or an integer"))?;
    if count == 0 {
        return Err(invalid_input("--warp-threads must be greater than zero"));
    }
    Ok(WarpThreads::Count(count))
}

fn parse_resampling(value: &str) -> io::Result<Resampling> {
    match value {
        "near" | "nearest" => Ok(Resampling::Nearest),
        "bilinear" => Ok(Resampling::Bilinear),
        "cubic" => Ok(Resampling::Cubic),
        "cubicspline" | "cubic_spline" => Ok(Resampling::CubicSpline),
        "lanczos" => Ok(Resampling::Lanczos),
        "average" => Ok(Resampling::Average),
        _ => Err(invalid_input(
            "--resampling must be nearest, bilinear, cubic, cubicspline, lanczos, or average",
        )),
    }
}

fn parse_strategy(value: &str) -> io::Result<StrategyPreference> {
    match value {
        "auto" => Ok(StrategyPreference::Auto),
        "same-crs" | "same_crs" => Ok(StrategyPreference::SameCrs),
        "geographic" | "wgs84" => Ok(StrategyPreference::Geographic),
        "gdal-warp" | "gdal_warp" => Ok(StrategyPreference::GdalWarp),
        _ => Err(invalid_input(
            "--strategy must be auto, same-crs, geographic, or gdal-warp",
        )),
    }
}

fn parse_name_value(value: &str) -> io::Result<(String, String)> {
    let Some((name, option_value)) = value.split_once('=') else {
        return Err(invalid_input("--warp-option must be NAME=VALUE"));
    };
    if name.is_empty() || option_value.is_empty() {
        return Err(invalid_input("--warp-option must be NAME=VALUE"));
    }
    Ok((name.to_owned(), option_value.to_owned()))
}

fn available_workers() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
}

fn print_plan(opts: &RasterOptions, jobs: &[TileJob]) {
    println!("Render Plan");
    print_field("Input", &opts.input.display().to_string());
    print_field("Output", &opts.output.display().to_string());
    print_field("Format", opts.format.as_str());
    print_field("Tile size", &format!("{} px", opts.tile_size));
    print_field("Workers", &opts.workers.to_string());
    print_field(
        "Warp memory",
        &format_bytes(opts.warp_memory_bytes.round() as u64),
    );
    print_field("Warp threads", &opts.warp_threads.label());
    print_field("Resampling", opts.resampling.label());
    print_field("Strategy", opts.strategy.label());
    if !opts.warp_options.is_empty() {
        println!();
        println!("Warp Options");
        for (name, value) in &opts.warp_options {
            print_field(name, value);
        }
    }
    println!();
    println!("Coverage");
    match opts.chunk_tiles {
        Some(chunk_tiles) => print_field("Chunk tiles", &chunk_tiles.to_string()),
        None => print_field("Chunk tiles", "disabled"),
    }
    print_field("Zooms", &format!("{}..{}", opts.min_zoom, opts.max_zoom));
    print_field(
        "Bounds",
        &format!(
            "{:.7}, {:.7}, {:.7}, {:.7}",
            opts.bounds[0], opts.bounds[1], opts.bounds[2], opts.bounds[3]
        ),
    );
    print_field("Tiles", &format_count(jobs.len() as u64));
    if let Some(first) = jobs.first() {
        let (minx, miny, maxx, maxy) = tile_bounds_mercator(first.z, first.x, first.y);
        print_field(
            "First tile",
            &format!("{minx:.3}, {miny:.3}, {maxx:.3}, {maxy:.3}"),
        );
    }
    println!();
    println!("Tiles by Zoom");
    for z in opts.min_zoom..=opts.max_zoom {
        let count = jobs.iter().filter(|job| job.z == z).count();
        print_field(&format!("z{z}"), &format_count(count as u64));
    }
}

fn render_native(opts: &RasterOptions, jobs: &[TileJob]) -> io::Result<()> {
    use gdal::Dataset;
    let source = Dataset::open(&opts.input).map_err(gdal_to_io)?;
    let strategy = select_render_strategy(&source, opts)?;
    eprintln!("Rendering");
    status_field("Input", &opts.input.display().to_string());
    status_field("Output", &opts.output.display().to_string());
    status_field("Zooms", &format!("{}..{}", opts.min_zoom, opts.max_zoom));
    status_field("Tiles", &format_tile_count(jobs.len()));
    status_field(
        "Format",
        &format!("{}, {} px", opts.format.as_str(), opts.tile_size),
    );
    status_field("Strategy", strategy.label());
    status_field(
        "Bounds",
        &format!(
            "{:.7}, {:.7}, {:.7}, {:.7}",
            opts.bounds[0], opts.bounds[1], opts.bounds[2], opts.bounds[3]
        ),
    );
    eprintln!();

    let mut writer = pmtiles::ArchiveWriter::create_with_reserved_directories(
        &opts.output,
        reserved_directory_bytes(jobs.len()),
    )?;
    let started = std::time::Instant::now();

    let mut completed = 0;
    for zoom_jobs in jobs_by_zoom(jobs) {
        let zoom = zoom_jobs[0].z;
        let mut rendered_zoom = match opts.chunk_tiles {
            Some(chunk_tiles) => render_zoom_chunked(&source, opts, zoom_jobs, chunk_tiles)?,
            None => render_zoom_wide(&source, opts, zoom_jobs)?,
        };
        rendered_zoom.tiles.sort_by_key(|tile| tile.tile_id);

        for rendered in rendered_zoom.tiles {
            writer.add_tile(
                rendered.tile_id,
                rendered.data.len().try_into().map_err(|_| {
                    invalid_input(format!(
                        "encoded tile exceeds PMTiles entry limit: {}",
                        rendered.tile_id
                    ))
                })?,
            );
            writer.write_tile_data(&rendered.data)?;
            completed += 1;
        }
        status_field(
            &format!("Zoom z{zoom}"),
            &format!(
                "done in {:.1}s ({} of {}, {:.0}%)",
                rendered_zoom.elapsed,
                format_count(completed as u64),
                format_count(jobs.len() as u64),
                completed as f64 / jobs.len() as f64 * 100.0
            ),
        );
    }

    let metadata = build_metadata(opts);
    let layout = writer.finish_reserved_directories(metadata.as_bytes())?;

    let header = pmtiles::Header {
        root_offset: pmtiles::HEADER_LEN,
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
        tile_type: opts.format.pmtiles_type(),
        min_zoom: opts.min_zoom,
        max_zoom: opts.max_zoom,
        min_lon_e7: to_e7(opts.bounds[0]),
        min_lat_e7: to_e7(opts.bounds[1]),
        max_lon_e7: to_e7(opts.bounds[2]),
        max_lat_e7: to_e7(opts.bounds[3]),
        center_zoom: opts.min_zoom,
        center_lon_e7: to_e7((opts.bounds[0] + opts.bounds[2]) / 2.0),
        center_lat_e7: to_e7((opts.bounds[1] + opts.bounds[3]) / 2.0),
    };
    writer.write_header(&header)?;
    print_render_summary(opts, jobs, started.elapsed().as_secs_f64())?;
    Ok(())
}

fn reserved_directory_bytes(tile_count: usize) -> u64 {
    let estimate = (tile_count as u64).saturating_mul(24);
    estimate.clamp(MIN_RESERVED_DIRECTORY_BYTES, MAX_RESERVED_DIRECTORY_BYTES)
}

fn render_zoom_wide(
    source: &gdal::Dataset,
    opts: &RasterOptions,
    zoom_jobs: &[TileJob],
) -> io::Result<RenderedZoom> {
    let zoom = zoom_jobs[0].z;
    let started = std::time::Instant::now();
    status_field(
        &format!("Zoom z{zoom}"),
        &format!("rendering {}", format_tile_count(zoom_jobs.len())),
    );
    let dataset = build_tile_dataset(source, opts, zoom_jobs)?;
    let tiles = render_chunk_tiles_parallel(opts, dataset, zoom_jobs)?;
    Ok(RenderedZoom {
        tiles,
        elapsed: started.elapsed().as_secs_f64(),
    })
}

fn render_zoom_chunked(
    source: &gdal::Dataset,
    opts: &RasterOptions,
    zoom_jobs: &[TileJob],
    chunk_tiles: u32,
) -> io::Result<RenderedZoom> {
    let zoom = zoom_jobs[0].z;
    let started = std::time::Instant::now();
    let chunks = chunk_jobs(zoom_jobs, chunk_tiles);
    let worker_count = opts.workers.max(1).min(chunks.len().max(1));
    let strategy = select_render_strategy(source, opts)?;
    let estimated_chunk_mib = estimate_chunk_working_set_mib(opts, zoom_jobs, chunk_tiles);
    status_field(
        &format!("Zoom z{zoom}"),
        &format!(
            "rendering {} in {} chunks ({} workers, {}, {})",
            format_tile_count(zoom_jobs.len()),
            format_count(chunks.len() as u64),
            worker_count,
            strategy.label(),
            format_bytes((estimated_chunk_mib * 1024.0 * 1024.0).round() as u64)
        ),
    );

    if worker_count == 1 || chunks.len() <= 1 {
        let worker_source = gdal::Dataset::open(&opts.input).map_err(gdal_to_io)?;
        let mut rendered_zoom = Vec::with_capacity(zoom_jobs.len());
        let mut chunk_opts = opts.clone();
        chunk_opts.workers = 1;
        for (chunk_index, chunk_jobs) in chunks.iter().enumerate() {
            let chunk_started = std::time::Instant::now();
            let chunk_dataset = build_tile_dataset(&worker_source, &chunk_opts, chunk_jobs)?;
            let mut rendered_chunk =
                render_chunk_tiles_parallel(&chunk_opts, chunk_dataset, chunk_jobs)?;
            status_field(
                &format!("Zoom z{zoom}"),
                &format!(
                    "chunk {}/{} done in {:.1}s ({})",
                    chunk_index + 1,
                    chunks.len(),
                    chunk_started.elapsed().as_secs_f64(),
                    format_tile_count(rendered_chunk.len())
                ),
            );
            rendered_zoom.append(&mut rendered_chunk);
        }
        return Ok(RenderedZoom {
            tiles: rendered_zoom,
            elapsed: started.elapsed().as_secs_f64(),
        });
    }

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    let next_index = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel();

    let rendered_by_chunk =
        std::thread::scope(|scope| -> io::Result<Vec<Option<Vec<RenderedTile>>>> {
            for _ in 0..worker_count {
                let tx = tx.clone();
                let next_index = &next_index;
                let chunks = &chunks;
                scope.spawn(move || {
                    let result: Result<(), String> = (|| {
                        let worker_source =
                            gdal::Dataset::open(&opts.input).map_err(|err| err.to_string())?;
                        let mut chunk_opts = opts.clone();
                        chunk_opts.workers = 1;

                        loop {
                            let chunk_index = next_index.fetch_add(1, Ordering::Relaxed);
                            if chunk_index >= chunks.len() {
                                break;
                            }
                            let chunk_jobs = &chunks[chunk_index];
                            let chunk_started = std::time::Instant::now();
                            let chunk_dataset =
                                build_tile_dataset(&worker_source, &chunk_opts, chunk_jobs)
                                    .map_err(|err| err.to_string())?;
                            let rendered_chunk =
                                render_chunk_tiles_parallel(&chunk_opts, chunk_dataset, chunk_jobs)
                                    .map_err(|err| err.to_string())?;
                            let elapsed = chunk_started.elapsed().as_secs_f64();
                            if tx.send(Ok((chunk_index, rendered_chunk, elapsed))).is_err() {
                                break;
                            }
                        }
                        Ok(())
                    })();
                    if let Err(err) = result {
                        let _ = tx.send(Err(err));
                    }
                });
            }
            drop(tx);

            let mut rendered_by_chunk = (0..chunks.len()).map(|_| None).collect::<Vec<_>>();
            let mut completed_chunks = 0;
            for result in rx {
                let (chunk_index, rendered_chunk, elapsed) = result.map_err(io::Error::other)?;
                completed_chunks += 1;
                status_field(
                    &format!("Zoom z{zoom}"),
                    &format!(
                        "chunk {}/{} done in {:.1}s ({})",
                        completed_chunks,
                        chunks.len(),
                        elapsed,
                        format_tile_count(rendered_chunk.len())
                    ),
                );
                rendered_by_chunk[chunk_index] = Some(rendered_chunk);
            }
            if completed_chunks != chunks.len() {
                return Err(io::Error::other("not all chunk workers completed"));
            }
            Ok(rendered_by_chunk)
        })?;

    let mut rendered_zoom = Vec::with_capacity(zoom_jobs.len());
    for rendered_chunk in rendered_by_chunk {
        let mut rendered_chunk =
            rendered_chunk.ok_or_else(|| io::Error::other("missing rendered chunk"))?;
        rendered_zoom.append(&mut rendered_chunk);
    }
    Ok(RenderedZoom {
        tiles: rendered_zoom,
        elapsed: started.elapsed().as_secs_f64(),
    })
}

fn estimate_chunk_working_set_mib(
    opts: &RasterOptions,
    zoom_jobs: &[TileJob],
    chunk_tiles: u32,
) -> f64 {
    let max_chunk_width_tiles = zoom_jobs
        .iter()
        .map(|job| job.x % chunk_tiles)
        .max()
        .unwrap_or(0)
        + 1;
    let max_chunk_height_tiles = zoom_jobs
        .iter()
        .map(|job| job.y % chunk_tiles)
        .max()
        .unwrap_or(0)
        + 1;
    let width = max_chunk_width_tiles as usize * opts.tile_size as usize;
    let height = max_chunk_height_tiles as usize * opts.tile_size as usize;
    let assumed_bands = 4_usize;

    // Fast paths keep planar output plus an interleaved read buffer in memory.
    let bytes = width
        .saturating_mul(height)
        .saturating_mul(assumed_bands)
        .saturating_mul(2);
    bytes as f64 / 1024.0 / 1024.0
}

fn render_chunk_tiles_parallel(
    opts: &RasterOptions,
    chunk_dataset: ChunkDataset,
    jobs: &[TileJob],
) -> io::Result<Vec<RenderedTile>> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, mpsc};

    let worker_count = opts.workers.max(1).min(jobs.len().max(1));
    if worker_count == 1 || jobs.len() <= 1 {
        let mut rendered = Vec::with_capacity(jobs.len());
        for job in jobs {
            let pixels = read_tile_pixels(opts, &chunk_dataset, job)?;
            let data = encode_tile_gdal(opts, job, pixels)?;
            rendered.push(RenderedTile {
                tile_id: job.tile_id,
                data,
            });
        }
        return Ok(rendered);
    }

    let chunk_dataset = Arc::new(Mutex::new(chunk_dataset));
    let next_index = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel();

    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            let tx = tx.clone();
            let chunk_dataset = Arc::clone(&chunk_dataset);
            let next_index = &next_index;
            scope.spawn(move || {
                loop {
                    let index = next_index.fetch_add(1, Ordering::Relaxed);
                    if index >= jobs.len() {
                        break;
                    }
                    let job = jobs[index];
                    let result: Result<RenderedTile, String> = (|| {
                        let pixels = {
                            let chunk_dataset = chunk_dataset
                                .lock()
                                .map_err(|_| "chunk dataset lock poisoned".to_owned())?;
                            read_tile_pixels(opts, &chunk_dataset, &job)
                                .map_err(|err| err.to_string())?
                        };
                        let data =
                            encode_tile_gdal(opts, &job, pixels).map_err(|err| err.to_string())?;
                        Ok(RenderedTile {
                            tile_id: job.tile_id,
                            data,
                        })
                    })();
                    if tx.send(result).is_err() {
                        break;
                    }
                }
            });
        }
    });
    drop(tx);

    let mut rendered = Vec::with_capacity(jobs.len());
    for result in rx {
        rendered.push(result.map_err(io::Error::other)?);
    }
    if rendered.len() != jobs.len() {
        return Err(io::Error::other("not all tile workers completed"));
    }
    rendered.sort_by_key(|tile| tile.tile_id);
    Ok(rendered)
}

fn build_tile_dataset(
    source: &gdal::Dataset,
    opts: &RasterOptions,
    jobs: &[TileJob],
) -> io::Result<ChunkDataset> {
    use gdal::DriverManager;
    use gdal::spatial_ref::SpatialRef;

    match select_render_strategy(source, opts)? {
        RenderStrategy::SameCrsWebMercator => {
            if let Some(dataset) = build_same_crs_webmercator_dataset(source, opts, jobs)? {
                return Ok(dataset);
            }
        }
        RenderStrategy::GeographicWgs84 => {
            if let Some(dataset) = build_fast_geographic_dataset(source, opts, jobs)? {
                return Ok(dataset);
            }
        }
        RenderStrategy::GdalWarp => {}
    }

    let band_count = source.raster_count().clamp(1, 4);
    let min_x = jobs.iter().map(|job| job.x).min().expect("non-empty jobs");
    let max_x = jobs.iter().map(|job| job.x).max().expect("non-empty jobs");
    let min_y = jobs.iter().map(|job| job.y).min().expect("non-empty jobs");
    let max_y = jobs.iter().map(|job| job.y).max().expect("non-empty jobs");
    let width = (max_x - min_x + 1) as usize * opts.tile_size as usize;
    let height = (max_y - min_y + 1) as usize * opts.tile_size as usize;

    let mem_driver = DriverManager::get_driver_by_name("MEM").map_err(gdal_to_io)?;
    let mut dataset = mem_driver
        .create_with_band_type::<u8, _>("", width, height, band_count)
        .map_err(gdal_to_io)?;

    let (left, _, _, top) = tile_bounds_mercator(jobs[0].z, min_x, min_y);
    let (_, bottom, right, _) = tile_bounds_mercator(jobs[0].z, max_x, max_y);
    dataset
        .set_geo_transform(&[
            left,
            (right - left) / width as f64,
            0.0,
            top,
            0.0,
            -((top - bottom) / height as f64),
        ])
        .map_err(gdal_to_io)?;
    let dst_srs = SpatialRef::from_epsg(3857).map_err(gdal_to_io)?;
    dataset.set_spatial_ref(&dst_srs).map_err(gdal_to_io)?;

    reproject_with_options(source, &dataset, opts)?;

    Ok(ChunkDataset::Gdal(GdalChunk {
        dataset,
        min_x,
        min_y,
        band_count,
    }))
}

fn select_render_strategy(
    source: &gdal::Dataset,
    opts: &RasterOptions,
) -> io::Result<RenderStrategy> {
    match opts.strategy {
        StrategyPreference::GdalWarp => return Ok(RenderStrategy::GdalWarp),
        StrategyPreference::SameCrs => return Ok(RenderStrategy::SameCrsWebMercator),
        StrategyPreference::Geographic => return Ok(RenderStrategy::GeographicWgs84),
        StrategyPreference::Auto => {}
    }

    if !matches!(opts.resampling, Resampling::Bilinear | Resampling::Nearest) {
        return Ok(RenderStrategy::GdalWarp);
    }
    let Ok(spatial_ref) = source.spatial_ref() else {
        return Ok(RenderStrategy::GdalWarp);
    };
    if is_web_mercator(&spatial_ref)? {
        return Ok(RenderStrategy::SameCrsWebMercator);
    }
    if is_wgs84_geographic(&spatial_ref)? {
        return Ok(RenderStrategy::GeographicWgs84);
    }
    Ok(RenderStrategy::GdalWarp)
}

fn is_web_mercator(spatial_ref: &gdal::spatial_ref::SpatialRef) -> io::Result<bool> {
    let web_mercator = gdal::spatial_ref::SpatialRef::from_epsg(3857).map_err(gdal_to_io)?;
    Ok(spatial_ref == &web_mercator || spatial_ref.auth_code().ok() == Some(3857))
}

fn is_wgs84_geographic(spatial_ref: &gdal::spatial_ref::SpatialRef) -> io::Result<bool> {
    let wgs84 = gdal::spatial_ref::SpatialRef::from_epsg(4326).map_err(gdal_to_io)?;
    Ok(spatial_ref == &wgs84 || spatial_ref.auth_code().ok() == Some(4326))
}

fn build_same_crs_webmercator_dataset(
    source: &gdal::Dataset,
    opts: &RasterOptions,
    jobs: &[TileJob],
) -> io::Result<Option<ChunkDataset>> {
    use gdal::raster::GdalDataType;

    if !matches!(opts.resampling, Resampling::Bilinear | Resampling::Nearest) {
        return Ok(None);
    }
    let Ok(spatial_ref) = source.spatial_ref() else {
        return Ok(None);
    };
    if !is_web_mercator(&spatial_ref)? {
        return Ok(None);
    }
    let Ok(transform) = source.geo_transform() else {
        return Ok(None);
    };
    if transform[2].abs() > 1e-12 || transform[4].abs() > 1e-12 {
        return Ok(None);
    }
    if transform[1] <= 0.0 || transform[5] >= 0.0 {
        return Ok(None);
    }

    let band_count = source.raster_count().clamp(1, 4);
    for band_index in 1..=band_count {
        let band = source.rasterband(band_index).map_err(gdal_to_io)?;
        if band.band_type() != GdalDataType::UInt8 {
            return Ok(None);
        }
    }

    let min_x = jobs.iter().map(|job| job.x).min().expect("non-empty jobs");
    let max_x = jobs.iter().map(|job| job.x).max().expect("non-empty jobs");
    let min_y = jobs.iter().map(|job| job.y).min().expect("non-empty jobs");
    let max_y = jobs.iter().map(|job| job.y).max().expect("non-empty jobs");
    let width = (max_x - min_x + 1) as usize * opts.tile_size as usize;
    let height = (max_y - min_y + 1) as usize * opts.tile_size as usize;

    let (left, _, _, top) = tile_bounds_mercator(jobs[0].z, min_x, min_y);
    let (_, bottom, right, _) = tile_bounds_mercator(jobs[0].z, max_x, max_y);

    {
        let source_width = unsafe { gdal_sys::GDALGetRasterXSize(source.c_dataset()) } as usize;
        let source_height = unsafe { gdal_sys::GDALGetRasterYSize(source.c_dataset()) } as usize;
        let mut band_map = (1..=band_count as i32).collect::<Vec<_>>();
        let mut pixels = vec![0_u8; width * height * band_count];

        let source_left = transform[0];
        let source_top = transform[3];
        let source_right = source_left + source_width as f64 * transform[1];
        let source_bottom = source_top + source_height as f64 * transform[5];
        let intersect_left = left.max(source_left);
        let intersect_right = right.min(source_right);
        let intersect_top = top.min(source_top);
        let intersect_bottom = bottom.max(source_bottom);

        if intersect_left < intersect_right && intersect_bottom < intersect_top {
            let dst_pixel_width = (right - left) / width as f64;
            let dst_pixel_height = (top - bottom) / height as f64;
            let dst_xoff = ((intersect_left - left) / dst_pixel_width)
                .floor()
                .clamp(0.0, width as f64) as usize;
            let dst_xend = ((intersect_right - left) / dst_pixel_width)
                .ceil()
                .clamp(0.0, width as f64) as usize;
            let dst_yoff = ((top - intersect_top) / dst_pixel_height)
                .floor()
                .clamp(0.0, height as f64) as usize;
            let dst_yend = ((top - intersect_bottom) / dst_pixel_height)
                .ceil()
                .clamp(0.0, height as f64) as usize;
            let dst_width = dst_xend.saturating_sub(dst_xoff);
            let dst_height = dst_yend.saturating_sub(dst_yoff);

            if dst_width > 0 && dst_height > 0 {
                let window_left = left + dst_xoff as f64 * dst_pixel_width;
                let window_right = left + dst_xend as f64 * dst_pixel_width;
                let window_top = top - dst_yoff as f64 * dst_pixel_height;
                let window_bottom = top - dst_yend as f64 * dst_pixel_height;
                let mut src_xoff = (window_left - source_left) / transform[1];
                let mut src_yoff = (window_top - source_top) / transform[5];
                let mut src_xsize = (window_right - window_left) / transform[1];
                let mut src_ysize = (window_bottom - window_top) / transform[5];
                if src_xoff < 0.0 {
                    src_xsize += src_xoff;
                    src_xoff = 0.0;
                }
                if src_yoff < 0.0 {
                    src_ysize += src_yoff;
                    src_yoff = 0.0;
                }
                src_xsize = src_xsize.min(source_width as f64 - src_xoff);
                src_ysize = src_ysize.min(source_height as f64 - src_yoff);

                let mut interleaved = vec![0_u8; dst_width * dst_height * band_count];
                read_interleaved_window_resampled(
                    source,
                    src_xoff,
                    src_yoff,
                    src_xsize,
                    src_ysize,
                    dst_width,
                    dst_height,
                    band_count,
                    &mut band_map,
                    opts.resampling,
                    &mut interleaved,
                )?;

                copy_interleaved_window(
                    &interleaved,
                    dst_width,
                    dst_height,
                    &mut pixels,
                    width,
                    dst_xoff,
                    dst_yoff,
                    band_count,
                );
            }
        }

        return Ok(Some(ChunkDataset::Native(NativeChunk {
            pixels,
            width,
            min_x,
            min_y,
            band_count,
        })));
    }
}

fn build_fast_geographic_dataset(
    source: &gdal::Dataset,
    opts: &RasterOptions,
    jobs: &[TileJob],
) -> io::Result<Option<ChunkDataset>> {
    use gdal::raster::GdalDataType;

    if !matches!(opts.resampling, Resampling::Bilinear | Resampling::Nearest) {
        return Ok(None);
    }
    let Ok(spatial_ref) = source.spatial_ref() else {
        return Ok(None);
    };
    if !is_wgs84_geographic(&spatial_ref)? {
        return Ok(None);
    }
    let Ok(transform) = source.geo_transform() else {
        return Ok(None);
    };
    if transform[2].abs() > 1e-12 || transform[4].abs() > 1e-12 {
        return Ok(None);
    }
    if transform[1] <= 0.0 || transform[5] >= 0.0 {
        return Ok(None);
    }

    let band_count = source.raster_count().clamp(1, 4);
    for band_index in 1..=band_count {
        let band = source.rasterband(band_index).map_err(gdal_to_io)?;
        if band.band_type() != GdalDataType::UInt8 {
            return Ok(None);
        }
    }

    let min_x = jobs.iter().map(|job| job.x).min().expect("non-empty jobs");
    let max_x = jobs.iter().map(|job| job.x).max().expect("non-empty jobs");
    let min_y = jobs.iter().map(|job| job.y).min().expect("non-empty jobs");
    let max_y = jobs.iter().map(|job| job.y).max().expect("non-empty jobs");
    let width = (max_x - min_x + 1) as usize * opts.tile_size as usize;
    let height = (max_y - min_y + 1) as usize * opts.tile_size as usize;

    let (left, _, _, top) = tile_bounds_mercator(jobs[0].z, min_x, min_y);
    let (_, bottom, right, _) = tile_bounds_mercator(jobs[0].z, max_x, max_y);

    let source_width = unsafe { gdal_sys::GDALGetRasterXSize(source.c_dataset()) } as usize;
    let source_height = unsafe { gdal_sys::GDALGetRasterYSize(source.c_dataset()) } as usize;
    let mut band_map = (1..=band_count as i32).collect::<Vec<_>>();
    let mut x_samples = Vec::with_capacity(width);
    for x in 0..width {
        let merc_x = left + (x as f64 + 0.5) * (right - left) / width as f64;
        let lon = merc_x / WEBMERC_MAX * 180.0;
        let src_x = (lon - transform[0]) / transform[1] - 0.5;
        x_samples.push(sample_indices(src_x, source_width, opts.resampling));
    }

    let mut pixels = vec![0_u8; width * height * band_count];
    let row_len = source_width * band_count;
    let mut row_a = vec![0_u8; row_len];
    let mut row_b = vec![0_u8; row_len];
    let mut cached_a = None;
    let mut cached_b = None;

    for y in 0..height {
        let merc_y = top - (y as f64 + 0.5) * (top - bottom) / height as f64;
        let lat = mercator_y_to_lat(merc_y);
        let src_y = (lat - transform[3]) / transform[5] - 0.5;
        let y_sample = sample_indices(src_y, source_height, opts.resampling);

        if cached_a != Some(y_sample.lower) {
            read_interleaved_row(
                source,
                y_sample.lower,
                source_width,
                band_count,
                &mut band_map,
                &mut row_a,
            )?;
            cached_a = Some(y_sample.lower);
        }
        if cached_b != Some(y_sample.upper) {
            if y_sample.upper == y_sample.lower {
                row_b.copy_from_slice(&row_a);
            } else {
                read_interleaved_row(
                    source,
                    y_sample.upper,
                    source_width,
                    band_count,
                    &mut band_map,
                    &mut row_b,
                )?;
            }
            cached_b = Some(y_sample.upper);
        }

        for (x, x_sample) in x_samples.iter().enumerate() {
            let out_offset = (y * width + x) * band_count;
            for band_offset in 0..band_count {
                let top_left = row_a[x_sample.lower * band_count + band_offset] as f64;
                let top_right = row_a[x_sample.upper * band_count + band_offset] as f64;
                let bottom_left = row_b[x_sample.lower * band_count + band_offset] as f64;
                let bottom_right = row_b[x_sample.upper * band_count + band_offset] as f64;
                let top_value = top_left + (top_right - top_left) * x_sample.weight;
                let bottom_value = bottom_left + (bottom_right - bottom_left) * x_sample.weight;
                pixels[out_offset + band_offset] =
                    (top_value + (bottom_value - top_value) * y_sample.weight).round() as u8;
            }
        }
    }

    Ok(Some(ChunkDataset::Native(NativeChunk {
        pixels,
        width,
        min_x,
        min_y,
        band_count,
    })))
}

fn copy_interleaved_window(
    source: &[u8],
    source_width: usize,
    source_height: usize,
    destination: &mut [u8],
    destination_width: usize,
    destination_x: usize,
    destination_y: usize,
    band_count: usize,
) {
    let row_bytes = source_width * band_count;
    for y in 0..source_height {
        let source_start = y * row_bytes;
        let destination_start =
            ((destination_y + y) * destination_width + destination_x) * band_count;
        destination[destination_start..destination_start + row_bytes]
            .copy_from_slice(&source[source_start..source_start + row_bytes]);
    }
}

#[derive(Clone, Copy)]
struct SampleIndex {
    lower: usize,
    upper: usize,
    weight: f64,
}

fn sample_indices(value: f64, size: usize, resampling: Resampling) -> SampleIndex {
    if size <= 1 {
        return SampleIndex {
            lower: 0,
            upper: 0,
            weight: 0.0,
        };
    }
    match resampling {
        Resampling::Nearest => {
            let index = value.round().clamp(0.0, (size - 1) as f64) as usize;
            SampleIndex {
                lower: index,
                upper: index,
                weight: 0.0,
            }
        }
        _ => {
            let lower = value.floor().clamp(0.0, (size - 1) as f64) as usize;
            let upper = (lower + 1).min(size - 1);
            SampleIndex {
                lower,
                upper,
                weight: (value - lower as f64).clamp(0.0, 1.0),
            }
        }
    }
}

fn mercator_y_to_lat(merc_y: f64) -> f64 {
    (2.0 * (merc_y / WEBMERC_MAX * std::f64::consts::PI).exp().atan() - std::f64::consts::FRAC_PI_2)
        .to_degrees()
}

fn read_interleaved_row(
    source: &gdal::Dataset,
    y: usize,
    width: usize,
    band_count: usize,
    band_map: &mut [i32],
    buffer: &mut [u8],
) -> io::Result<()> {
    read_interleaved_row_window(source, 0, y, width, band_count, band_map, buffer)
}

fn read_interleaved_row_window(
    source: &gdal::Dataset,
    x: usize,
    y: usize,
    width: usize,
    band_count: usize,
    band_map: &mut [i32],
    buffer: &mut [u8],
) -> io::Result<()> {
    let result = unsafe {
        gdal_sys::GDALDatasetRasterIO(
            source.c_dataset(),
            gdal_sys::GDALRWFlag::GF_Read,
            x.try_into()
                .map_err(|_| invalid_input("source column exceeds GDAL int range"))?,
            y.try_into()
                .map_err(|_| invalid_input("source row exceeds GDAL int range"))?,
            width
                .try_into()
                .map_err(|_| invalid_input("source width exceeds GDAL int range"))?,
            1,
            buffer.as_mut_ptr() as *mut std::ffi::c_void,
            width
                .try_into()
                .map_err(|_| invalid_input("source width exceeds GDAL int range"))?,
            1,
            gdal_sys::GDALDataType::GDT_Byte,
            band_count
                .try_into()
                .map_err(|_| invalid_input("band count exceeds GDAL int range"))?,
            band_map.as_mut_ptr(),
            band_count
                .try_into()
                .map_err(|_| invalid_input("pixel stride exceeds GDAL int range"))?,
            (width * band_count)
                .try_into()
                .map_err(|_| invalid_input("line stride exceeds GDAL int range"))?,
            1,
        )
    };
    if result != gdal_sys::CPLErr::CE_None {
        return Err(io::Error::other(last_gdal_error_message()));
    }
    Ok(())
}

fn read_interleaved_window_resampled(
    source: &gdal::Dataset,
    src_xoff: f64,
    src_yoff: f64,
    src_xsize: f64,
    src_ysize: f64,
    dst_width: usize,
    dst_height: usize,
    band_count: usize,
    band_map: &mut [i32],
    resampling: Resampling,
    buffer: &mut [u8],
) -> io::Result<()> {
    let rio_resampling = match resampling {
        Resampling::Nearest => gdal_sys::GDALRIOResampleAlg::GRIORA_NearestNeighbour,
        _ => gdal_sys::GDALRIOResampleAlg::GRIORA_Bilinear,
    };
    let mut extra = gdal_sys::GDALRasterIOExtraArg {
        nVersion: 1,
        eResampleAlg: rio_resampling,
        pfnProgress: None,
        pProgressData: std::ptr::null_mut(),
        bFloatingPointWindowValidity: 1,
        dfXOff: src_xoff,
        dfYOff: src_yoff,
        dfXSize: src_xsize,
        dfYSize: src_ysize,
    };
    let int_xoff = src_xoff.floor().max(0.0) as i32;
    let int_yoff = src_yoff.floor().max(0.0) as i32;
    let int_xsize = src_xsize.ceil().max(1.0) as i32;
    let int_ysize = src_ysize.ceil().max(1.0) as i32;
    let result = unsafe {
        gdal_sys::GDALDatasetRasterIOEx(
            source.c_dataset(),
            gdal_sys::GDALRWFlag::GF_Read,
            int_xoff,
            int_yoff,
            int_xsize,
            int_ysize,
            buffer.as_mut_ptr() as *mut std::ffi::c_void,
            dst_width
                .try_into()
                .map_err(|_| invalid_input("destination width exceeds GDAL int range"))?,
            dst_height
                .try_into()
                .map_err(|_| invalid_input("destination height exceeds GDAL int range"))?,
            gdal_sys::GDALDataType::GDT_Byte,
            band_count
                .try_into()
                .map_err(|_| invalid_input("band count exceeds GDAL int range"))?,
            band_map.as_mut_ptr(),
            band_count
                .try_into()
                .map_err(|_| invalid_input("pixel stride exceeds GDAL range"))?,
            (dst_width * band_count)
                .try_into()
                .map_err(|_| invalid_input("line stride exceeds GDAL range"))?,
            1,
            &mut extra,
        )
    };
    if result != gdal_sys::CPLErr::CE_None {
        return Err(io::Error::other(last_gdal_error_message()));
    }
    Ok(())
}

fn reproject_with_options(
    source: &gdal::Dataset,
    destination: &gdal::Dataset,
    opts: &RasterOptions,
) -> io::Result<()> {
    let mut warp_options = CslStringList::new();
    warp_options
        .set_name_value("NUM_THREADS", &opts.warp_threads.as_gdal_value())
        .map_err(gdal_to_io)?;
    warp_options
        .set_name_value("SKIP_NOSOURCE", "YES")
        .map_err(gdal_to_io)?;
    for (name, value) in &opts.warp_options {
        warp_options
            .set_name_value(name, value)
            .map_err(gdal_to_io)?;
    }

    let raw_options = unsafe { gdal_sys::GDALCreateWarpOptions() };
    if raw_options.is_null() {
        return Err(io::Error::other("GDALCreateWarpOptions failed"));
    }

    unsafe {
        (*raw_options).papszWarpOptions = warp_options.into_ptr() as *mut *mut std::ffi::c_char;
    }

    let result = unsafe {
        gdal_sys::CPLErrorReset();
        gdal_sys::GDALReprojectImage(
            source.c_dataset(),
            std::ptr::null(),
            destination.c_dataset(),
            std::ptr::null(),
            opts.resampling.as_gdal(),
            opts.warp_memory_bytes,
            0.0,
            None,
            std::ptr::null_mut(),
            raw_options,
        )
    };
    unsafe {
        gdal_sys::GDALDestroyWarpOptions(raw_options);
    }

    if result != gdal_sys::CPLErr::CE_None {
        return Err(io::Error::other(last_gdal_error_message()));
    }
    Ok(())
}

fn last_gdal_error_message() -> String {
    unsafe {
        let message = gdal_sys::CPLGetLastErrorMsg();
        if message.is_null() {
            return "GDAL warp failed".to_owned();
        }
        let message = CStr::from_ptr(message).to_string_lossy();
        if message.is_empty() {
            "GDAL warp failed".to_owned()
        } else {
            message.into_owned()
        }
    }
}

fn read_tile_pixels(
    opts: &RasterOptions,
    chunk_dataset: &ChunkDataset,
    job: &TileJob,
) -> io::Result<TilePixels> {
    let tile_size = opts.tile_size as usize;
    match chunk_dataset {
        ChunkDataset::Native(chunk) => {
            let xoff = ((job.x - chunk.min_x) * opts.tile_size) as usize;
            let yoff = ((job.y - chunk.min_y) * opts.tile_size) as usize;
            let mut pixels = vec![0_u8; tile_size * tile_size * chunk.band_count];
            let row_bytes = tile_size * chunk.band_count;
            for y in 0..tile_size {
                let source_start = ((yoff + y) * chunk.width + xoff) * chunk.band_count;
                let target_start = y * row_bytes;
                pixels[target_start..target_start + row_bytes]
                    .copy_from_slice(&chunk.pixels[source_start..source_start + row_bytes]);
            }
            Ok(TilePixels {
                tile_size,
                band_count: chunk.band_count,
                pixels,
            })
        }
        ChunkDataset::Gdal(chunk) => {
            let xoff = ((job.x - chunk.min_x) * opts.tile_size) as isize;
            let yoff = ((job.y - chunk.min_y) * opts.tile_size) as isize;
            let mut bands = Vec::with_capacity(chunk.band_count);
            for band_index in 1..=chunk.band_count {
                let source_band = chunk.dataset.rasterband(band_index).map_err(gdal_to_io)?;
                let data = source_band
                    .read_as::<u8>(
                        (xoff, yoff),
                        (tile_size, tile_size),
                        (tile_size, tile_size),
                        None,
                    )
                    .map_err(gdal_to_io)?;
                bands.push(data.data().to_vec());
            }
            Ok(TilePixels {
                tile_size,
                band_count: chunk.band_count,
                pixels: interleave_bands(&bands, tile_size * tile_size),
            })
        }
    }
}

fn encode_tile_gdal(
    opts: &RasterOptions,
    job: &TileJob,
    pixels: TilePixels,
) -> io::Result<Vec<u8>> {
    use gdal::DriverManager;
    use gdal::raster::{Buffer, RasterCreationOptions};

    if opts.format == RasterFormat::Webp && matches!(pixels.band_count, 3 | 4) {
        return encode_tile_webp(&pixels);
    }

    let mem_driver = DriverManager::get_driver_by_name("MEM").map_err(gdal_to_io)?;
    let tile_ds = mem_driver
        .create_with_band_type::<u8, _>("", pixels.tile_size, pixels.tile_size, pixels.band_count)
        .map_err(gdal_to_io)?;

    for band_offset in 0..pixels.band_count {
        let band_index = band_offset + 1;
        let band_data = deinterleave_band(&pixels.pixels, pixels.band_count, band_offset);
        let mut data = Buffer::new((pixels.tile_size, pixels.tile_size), band_data);
        let mut target_band = tile_ds.rasterband(band_index).map_err(gdal_to_io)?;
        target_band
            .write((0, 0), (pixels.tile_size, pixels.tile_size), &mut data)
            .map_err(gdal_to_io)?;
    }

    let driver_name = match opts.format {
        RasterFormat::Png => "PNG",
        RasterFormat::Jpeg => "JPEG",
        RasterFormat::Webp => "WEBP",
    };
    let image_driver = DriverManager::get_driver_by_name(driver_name).map_err(gdal_to_io)?;
    let mut creation_options = RasterCreationOptions::default();
    if matches!(opts.format, RasterFormat::Jpeg | RasterFormat::Webp) {
        creation_options
            .add_name_value("QUALITY", "85")
            .map_err(gdal_to_io)?;
    }

    let vsimem_path = format!(
        "/vsimem/pmtiler-{}-{}-{}-{}.{}",
        std::process::id(),
        job.z,
        job.x,
        job.y,
        opts.format.as_str()
    );
    let encoded = tile_ds
        .create_copy(&image_driver, &vsimem_path, &creation_options)
        .map_err(gdal_to_io)?;
    drop(encoded);
    gdal::vsi::get_vsi_mem_file_bytes_owned(&vsimem_path).map_err(gdal_to_io)
}

fn encode_tile_webp(pixels: &TilePixels) -> io::Result<Vec<u8>> {
    let width = i32::try_from(pixels.tile_size)
        .map_err(|_| invalid_input("tile width exceeds libwebp int range"))?;
    let height = i32::try_from(pixels.tile_size)
        .map_err(|_| invalid_input("tile height exceeds libwebp int range"))?;
    let stride = i32::try_from(pixels.tile_size * pixels.band_count)
        .map_err(|_| invalid_input("tile stride exceeds libwebp int range"))?;
    let mut output = std::ptr::null_mut();
    let size = unsafe {
        if pixels.band_count == 4 {
            WebPEncodeRGBA(
                pixels.pixels.as_ptr(),
                width,
                height,
                stride,
                85.0,
                &mut output,
            )
        } else {
            WebPEncodeRGB(
                pixels.pixels.as_ptr(),
                width,
                height,
                stride,
                85.0,
                &mut output,
            )
        }
    };
    if size == 0 || output.is_null() {
        return Err(io::Error::other("libwebp encode failed"));
    }

    let encoded = unsafe { std::slice::from_raw_parts(output, size).to_vec() };
    unsafe {
        WebPFree(output.cast());
    }
    Ok(encoded)
}

fn interleave_bands(bands: &[Vec<u8>], pixel_count: usize) -> Vec<u8> {
    let band_count = bands.len();
    let mut pixels = vec![0_u8; pixel_count * band_count];
    for pixel_index in 0..pixel_count {
        for band_offset in 0..band_count {
            pixels[pixel_index * band_count + band_offset] = bands[band_offset][pixel_index];
        }
    }
    pixels
}

fn deinterleave_band(pixels: &[u8], band_count: usize, band_offset: usize) -> Vec<u8> {
    pixels
        .chunks_exact(band_count)
        .map(|pixel| pixel[band_offset])
        .collect()
}

struct RenderedTile {
    tile_id: u64,
    data: Vec<u8>,
}

struct RenderedZoom {
    tiles: Vec<RenderedTile>,
    elapsed: f64,
}

struct TilePixels {
    tile_size: usize,
    band_count: usize,
    pixels: Vec<u8>,
}

enum ChunkDataset {
    Native(NativeChunk),
    Gdal(GdalChunk),
}

struct NativeChunk {
    pixels: Vec<u8>,
    width: usize,
    min_x: u32,
    min_y: u32,
    band_count: usize,
}

struct GdalChunk {
    dataset: gdal::Dataset,
    min_x: u32,
    min_y: u32,
    band_count: usize,
}

fn chunk_jobs(jobs: &[TileJob], chunk_tiles: u32) -> Vec<Vec<TileJob>> {
    let mut chunks_by_key: BTreeMap<(u32, u32), Vec<TileJob>> = BTreeMap::new();
    for job in jobs {
        let key = (job.x / chunk_tiles, job.y / chunk_tiles);
        chunks_by_key.entry(key).or_default().push(*job);
    }

    let mut chunks = Vec::with_capacity(chunks_by_key.len());
    for (_, mut chunk) in chunks_by_key {
        chunk.sort_by_key(|job| job.tile_id);
        chunks.push(chunk);
    }
    chunks
}

fn jobs_by_zoom(jobs: &[TileJob]) -> impl Iterator<Item = &[TileJob]> {
    let mut ranges = Vec::new();
    let mut start = 0;
    while start < jobs.len() {
        let zoom = jobs[start].z;
        let mut end = start + 1;
        while end < jobs.len() && jobs[end].z == zoom {
            end += 1;
        }
        ranges.push(&jobs[start..end]);
        start = end;
    }
    ranges.into_iter()
}

fn gdal_to_io(err: gdal::errors::GdalError) -> io::Error {
    io::Error::other(err.to_string())
}

fn build_metadata(opts: &RasterOptions) -> String {
    let name = opts
        .output
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "pmtiler raster".to_owned());
    format!(
        "{{\"name\":\"{}\",\"description\":\"{}\",\"attribution\":\"\",\"type\":\"overlay\",\"version\":\"1.0.0\",\"format\":\"{}\",\"tileSize\":{},\"minzoom\":{},\"maxzoom\":{},\"bounds\":[{:.7},{:.7},{:.7},{:.7}],\"center\":[{:.7},{:.7},{}]}}",
        json_escape(&name),
        json_escape(&opts.input.display().to_string()),
        opts.format.as_str(),
        opts.tile_size,
        opts.min_zoom,
        opts.max_zoom,
        opts.bounds[0],
        opts.bounds[1],
        opts.bounds[2],
        opts.bounds[3],
        (opts.bounds[0] + opts.bounds[2]) / 2.0,
        (opts.bounds[1] + opts.bounds[3]) / 2.0,
        opts.min_zoom
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

fn print_render_summary(opts: &RasterOptions, jobs: &[TileJob], elapsed: f64) -> io::Result<()> {
    let size = std::fs::metadata(&opts.output)?.len();
    let tiles_per_second = if elapsed > 0.0 {
        jobs.len() as f64 / elapsed
    } else {
        0.0
    };

    println!("Created {}", opts.output.display());
    print_field("Tiles", &format_count(jobs.len() as u64));
    print_field("Zooms", &format!("{}..{}", opts.min_zoom, opts.max_zoom));
    print_field("Format", opts.format.as_str());
    print_field("Tile size", &format!("{} px", opts.tile_size));
    print_field("Size", &format_bytes(size));
    print_field("Elapsed", &format!("{elapsed:.1}s"));
    print_field("Throughput", &format!("{tiles_per_second:.1} tiles/s"));
    Ok(())
}

fn print_field(label: &str, value: &str) {
    println!("  {label:<20} {value}");
}

fn status_field(label: &str, value: &str) {
    eprintln!("  {label:<18} {value}");
}

fn format_tile_count(count: usize) -> String {
    let suffix = if count == 1 { "tile" } else { "tiles" };
    format!("{} {suffix}", format_count(count as u64))
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

fn to_e7(value: f64) -> i32 {
    (value * 10_000_000.0).round() as i32
}

pub fn print_raster_help() {
    println!(
        "\
pmtiler raster

Render a GDAL-readable raster source into a PMTiles archive.

Usage:
  pmtiler raster <RASTER_OR_VRT> <OUTPUT.pmtiles> --zoom MIN-MAX [OPTIONS]

Required:
  --zoom <z|min-max>     Zoom or zoom range, for example 0-13

Options:
  --plan                 Print the tile job plan without rendering
  --bounds <w,s,e,n>     Override inferred lon/lat bounds
  --format <fmt>         png, jpeg, jpg, or webp [default: webp]
  --tile-size <px>       Output tile size [default: 512]
  --workers <n>          Native render workers [default: host parallelism]
  --chunk-tiles <n|off>  Chunk width/height in tiles, or disabled/off [default: 8]
  --warp-memory <size>   GDAL warp memory, suffix K/M/G allowed [default: 512M]
  --warp-threads <n|all> GDAL warp compute threads [default: all]
  --resampling <method>  nearest, bilinear, cubic, cubicspline, lanczos, average [default: bilinear]
  --strategy <strategy>  auto, same-crs, geographic, or gdal-warp [default: auto]
  --warp-option <K=V>    Extra GDAL warp option, repeatable
  -h, --help             Show help
"
    );
}

fn expect_arg<'a>(args: &'a [String], index: usize, option: &str) -> io::Result<&'a str> {
    args.get(index)
        .map(String::as_str)
        .ok_or_else(|| invalid_input(format!("{option} requires a value")))
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn world_bounds_at_z0_have_one_job() {
        let jobs =
            enumerate_tile_jobs([-180.0, -85.051_128_78, 180.0, 85.051_128_78], 0, 0).unwrap();
        assert_eq!(
            jobs,
            vec![TileJob {
                z: 0,
                x: 0,
                y: 0,
                tile_id: 0
            }]
        );
    }

    #[test]
    fn jobs_are_sorted_by_pmtiles_id() {
        let jobs =
            enumerate_tile_jobs([-180.0, -85.051_128_78, 180.0, 85.051_128_78], 0, 1).unwrap();
        let ids = jobs.iter().map(|job| job.tile_id).collect::<Vec<_>>();
        assert_eq!(ids, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn conus_counts_are_reasonable() {
        let jobs = enumerate_tile_jobs([-125.0, 24.0, -66.0, 50.0], 0, 3).unwrap();
        let z3 = jobs.iter().filter(|job| job.z == 3).count();
        assert_eq!(z3, 4);
    }

    #[test]
    fn tile_bounds_are_web_mercator() {
        let (minx, miny, maxx, maxy) = tile_bounds_mercator(0, 0, 0);
        assert!((minx + WEBMERC_MAX).abs() < 0.000_001);
        assert!((miny + WEBMERC_MAX).abs() < 0.000_001);
        assert!((maxx - WEBMERC_MAX).abs() < 0.000_001);
        assert!((maxy - WEBMERC_MAX).abs() < 0.000_001);
    }

    #[test]
    fn strategy_parser_accepts_expected_values() {
        assert_eq!(parse_strategy("auto").unwrap(), StrategyPreference::Auto);
        assert_eq!(
            parse_strategy("same-crs").unwrap(),
            StrategyPreference::SameCrs
        );
        assert_eq!(
            parse_strategy("geographic").unwrap(),
            StrategyPreference::Geographic
        );
        assert_eq!(
            parse_strategy("gdal-warp").unwrap(),
            StrategyPreference::GdalWarp
        );
    }

    #[test]
    fn crs_helpers_identify_fast_path_sources() {
        let web_mercator = gdal::spatial_ref::SpatialRef::from_epsg(3857).unwrap();
        let wgs84 = gdal::spatial_ref::SpatialRef::from_epsg(4326).unwrap();
        assert!(is_web_mercator(&web_mercator).unwrap());
        assert!(!is_web_mercator(&wgs84).unwrap());
        assert!(is_wgs84_geographic(&wgs84).unwrap());
        assert!(!is_wgs84_geographic(&web_mercator).unwrap());
    }
}
