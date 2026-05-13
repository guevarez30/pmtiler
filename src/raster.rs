use std::ffi::{CStr, c_int, c_void};
use std::io;
use std::path::{Path, PathBuf};

use crate::pmtiles;
use gdal::config::set_config_option;
use gdal::cpl::CslStringList;
use gdal::spatial_ref::{AxisMappingStrategy, CoordTransform, SpatialRef};
use gdal::{DatasetOptions, GdalOpenFlags, GeoTransformEx};
use std::collections::BTreeMap;

const WEBMERC_MAX: f64 = 20_037_508.342_789_244;
const WEBMERC_SIZE: f64 = WEBMERC_MAX * 2.0;
const DEFAULT_AUTO_CHUNK_TARGETS_PER_WORKER: usize = 8;
const DEFAULT_WEBP_QUALITY: f32 = 100.0;
const DEFAULT_WEBP_METHOD: i32 = 4;
const AUTO_CHUNK_MAX_BYTES: usize = 256 * 1024 * 1024;
const AUTO_CHUNK_MIN_BYTES: usize = 32 * 1024 * 1024;
const MIN_RESERVED_DIRECTORY_BYTES: u64 = 64 * 1024;
const MAX_RESERVED_DIRECTORY_BYTES: u64 = 16 * 1024 * 1024;
const WEBP_ENCODER_ABI_VERSION: c_int = 0x020f;

type WebPWriterFunction = Option<
    unsafe extern "C" fn(data: *const u8, data_size: usize, picture: *const WebPPicture) -> c_int,
>;

#[repr(C)]
struct WebPConfig {
    lossless: c_int,
    quality: f32,
    method: c_int,
    image_hint: c_int,
    target_size: c_int,
    target_psnr: f32,
    segments: c_int,
    sns_strength: c_int,
    filter_strength: c_int,
    filter_sharpness: c_int,
    filter_type: c_int,
    autofilter: c_int,
    alpha_compression: c_int,
    alpha_filtering: c_int,
    alpha_quality: c_int,
    pass: c_int,
    show_compressed: c_int,
    preprocessing: c_int,
    partitions: c_int,
    partition_limit: c_int,
    emulate_jpeg_size: c_int,
    thread_level: c_int,
    low_memory: c_int,
    near_lossless: c_int,
    exact: c_int,
    use_delta_palette: c_int,
    use_sharp_yuv: c_int,
    qmin: c_int,
    qmax: c_int,
}

#[repr(C)]
struct WebPMemoryWriter {
    mem: *mut u8,
    size: usize,
    max_size: usize,
    pad: [u32; 1],
}

#[repr(C)]
struct WebPPicture {
    use_argb: c_int,
    colorspace: c_int,
    width: c_int,
    height: c_int,
    y: *mut u8,
    u: *mut u8,
    v: *mut u8,
    y_stride: c_int,
    uv_stride: c_int,
    a: *mut u8,
    a_stride: c_int,
    pad1: [u32; 2],
    argb: *mut u32,
    argb_stride: c_int,
    pad2: [u32; 3],
    writer: WebPWriterFunction,
    custom_ptr: *mut c_void,
    extra_info_type: c_int,
    extra_info: *mut u8,
    stats: *mut c_void,
    error_code: c_int,
    progress_hook:
        Option<unsafe extern "C" fn(percent: c_int, picture: *const WebPPicture) -> c_int>,
    user_data: *mut c_void,
    pad3: [u32; 3],
    pad4: *mut u8,
    pad5: *mut u8,
    pad6: [u32; 8],
    memory: *mut c_void,
    memory_argb: *mut c_void,
    pad7: [*mut c_void; 2],
}

#[link(name = "webp")]
unsafe extern "C" {
    fn WebPConfigInitInternal(
        config: *mut WebPConfig,
        preset: c_int,
        quality: f32,
        version: c_int,
    ) -> c_int;
    fn WebPValidateConfig(config: *const WebPConfig) -> c_int;
    fn WebPPictureInitInternal(picture: *mut WebPPicture, version: c_int) -> c_int;
    fn WebPPictureImportRGB(picture: *mut WebPPicture, rgb: *const u8, rgb_stride: c_int) -> c_int;
    fn WebPPictureImportRGBA(
        picture: *mut WebPPicture,
        rgba: *const u8,
        rgba_stride: c_int,
    ) -> c_int;
    fn WebPPictureFree(picture: *mut WebPPicture);
    fn WebPMemoryWriterInit(writer: *mut WebPMemoryWriter);
    fn WebPMemoryWriterClear(writer: *mut WebPMemoryWriter);
    fn WebPMemoryWrite(data: *const u8, data_size: usize, picture: *const WebPPicture) -> c_int;
    fn WebPEncode(config: *const WebPConfig, picture: *mut WebPPicture) -> c_int;
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
    pub chunk_tiles: ChunkTiles,
    pub warp_memory_bytes: f64,
    pub warp_threads: WarpThreads,
    pub resampling: Resampling,
    pub strategy: StrategyPreference,
    pub webp_quality: f32,
    pub webp_method: i32,
    pub gdal_tuning: GdalTuning,
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
    Auto,
    All,
    Count(usize),
}

impl WarpThreads {
    fn as_gdal_value(self, concurrent_warps: usize) -> String {
        match self {
            Self::Auto => {
                if concurrent_warps <= 1 {
                    "ALL_CPUS".to_owned()
                } else {
                    (available_workers() / concurrent_warps.max(1))
                        .max(1)
                        .to_string()
                }
            }
            Self::All => "ALL_CPUS".to_owned(),
            Self::Count(count) => count.to_string(),
        }
    }

    fn label(self) -> String {
        match self {
            Self::Auto => "auto".to_owned(),
            Self::All => "all".to_owned(),
            Self::Count(count) => count.to_string(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChunkTiles {
    Auto,
    Fixed(u32),
    Disabled,
}

impl ChunkTiles {
    fn label(self) -> String {
        match self {
            Self::Auto => "auto".to_owned(),
            Self::Fixed(chunk_tiles) => chunk_tiles.to_string(),
            Self::Disabled => "disabled".to_owned(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct GdalTuning {
    pub cache_bytes: Option<u64>,
    pub config_options: Vec<(String, String)>,
    pub open_options: Vec<(String, String)>,
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
        print_plan(&opts, &jobs)?;
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
    let mut chunk_tiles = ChunkTiles::Auto;
    let mut warp_memory_bytes = 512.0 * 1024.0 * 1024.0;
    let mut warp_threads = WarpThreads::Auto;
    let mut resampling = Resampling::Bilinear;
    let mut strategy = StrategyPreference::Auto;
    let mut webp_quality = DEFAULT_WEBP_QUALITY;
    let mut webp_method = DEFAULT_WEBP_METHOD;
    let mut gdal_tuning = GdalTuning::default();
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
            "--quality" | "--webp-quality" => {
                i += 1;
                webp_quality = parse_webp_quality(expect_arg(&args, i, "--quality")?)?;
            }
            arg if arg.starts_with("--quality=") => {
                webp_quality = parse_webp_quality(&arg["--quality=".len()..])?;
            }
            arg if arg.starts_with("--webp-quality=") => {
                webp_quality = parse_webp_quality(&arg["--webp-quality=".len()..])?;
            }
            "--webp-method" => {
                i += 1;
                webp_method = parse_webp_method(expect_arg(&args, i, "--webp-method")?)?;
            }
            arg if arg.starts_with("--webp-method=") => {
                webp_method = parse_webp_method(&arg["--webp-method=".len()..])?;
            }
            "--warp-option" => {
                i += 1;
                warp_options.push(parse_name_value(expect_arg(&args, i, "--warp-option")?)?);
            }
            arg if arg.starts_with("--warp-option=") => {
                warp_options.push(parse_name_value(&arg["--warp-option=".len()..])?);
            }
            "--gdal-cache" => {
                i += 1;
                gdal_tuning.cache_bytes =
                    Some(parse_memory_bytes(expect_arg(&args, i, "--gdal-cache")?)?.round() as u64);
            }
            arg if arg.starts_with("--gdal-cache=") => {
                gdal_tuning.cache_bytes =
                    Some(parse_memory_bytes(&arg["--gdal-cache=".len()..])?.round() as u64);
            }
            "--gdal-config" => {
                i += 1;
                gdal_tuning.config_options.push(parse_name_value(expect_arg(
                    &args,
                    i,
                    "--gdal-config",
                )?)?);
            }
            arg if arg.starts_with("--gdal-config=") => {
                gdal_tuning
                    .config_options
                    .push(parse_name_value(&arg["--gdal-config=".len()..])?);
            }
            "--open-option" => {
                i += 1;
                gdal_tuning.open_options.push(parse_name_value(expect_arg(
                    &args,
                    i,
                    "--open-option",
                )?)?);
            }
            arg if arg.starts_with("--open-option=") => {
                gdal_tuning
                    .open_options
                    .push(parse_name_value(&arg["--open-option=".len()..])?);
            }
            "--gdal-disable-readdir" => {
                i += 1;
                let value =
                    parse_gdal_disable_readdir(expect_arg(&args, i, "--gdal-disable-readdir")?)?;
                gdal_tuning
                    .config_options
                    .push(("GDAL_DISABLE_READDIR_ON_OPEN".to_owned(), value));
            }
            arg if arg.starts_with("--gdal-disable-readdir=") => {
                let value = parse_gdal_disable_readdir(&arg["--gdal-disable-readdir=".len()..])?;
                gdal_tuning
                    .config_options
                    .push(("GDAL_DISABLE_READDIR_ON_OPEN".to_owned(), value));
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
    apply_gdal_tuning(&gdal_tuning)?;
    let bounds = match bounds {
        Some(bounds) => bounds,
        None => infer_raster_bounds(&input, &gdal_tuning)?,
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
        webp_quality,
        webp_method,
        gdal_tuning,
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

fn infer_raster_bounds(path: &Path, gdal_tuning: &GdalTuning) -> io::Result<[f64; 4]> {
    let dataset = open_raster_dataset(path, gdal_tuning)?;
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

fn apply_gdal_tuning(tuning: &GdalTuning) -> io::Result<()> {
    if let Some(cache_bytes) = tuning.cache_bytes {
        unsafe {
            gdal_sys::GDALSetCacheMax64(cache_bytes as gdal_sys::GIntBig);
        }
        set_config_option("GDAL_CACHEMAX", &cache_bytes.to_string()).map_err(gdal_to_io)?;
    }

    for (name, value) in &tuning.config_options {
        set_config_option(name, value).map_err(gdal_to_io)?;
    }
    Ok(())
}

fn open_raster_dataset(path: &Path, tuning: &GdalTuning) -> io::Result<gdal::Dataset> {
    let open_options = tuning
        .open_options
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>();
    let open_option_refs = open_options.iter().map(String::as_str).collect::<Vec<_>>();
    let options = DatasetOptions {
        open_flags: GdalOpenFlags::GDAL_OF_RASTER | GdalOpenFlags::GDAL_OF_VERBOSE_ERROR,
        open_options: if open_option_refs.is_empty() {
            None
        } else {
            Some(&open_option_refs)
        },
        ..DatasetOptions::default()
    };
    gdal::Dataset::open_ex(path, options).map_err(gdal_to_io)
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

fn parse_chunk_tiles(value: &str) -> io::Result<ChunkTiles> {
    if value.eq_ignore_ascii_case("auto") {
        return Ok(ChunkTiles::Auto);
    }
    if matches!(value, "disabled" | "disable" | "none" | "off") {
        return Ok(ChunkTiles::Disabled);
    }
    let chunk_tiles = value
        .parse()
        .map_err(|_| invalid_input("--chunk-tiles must be auto, an integer, or disabled"))?;
    if chunk_tiles == 0 {
        return Err(invalid_input(
            "--chunk-tiles must be greater than zero or disabled",
        ));
    }
    Ok(ChunkTiles::Fixed(chunk_tiles))
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
    if value.eq_ignore_ascii_case("auto") {
        return Ok(WarpThreads::Auto);
    }
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

fn parse_gdal_disable_readdir(value: &str) -> io::Result<String> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" => Ok("TRUE".to_owned()),
        "false" | "no" | "off" => Ok("FALSE".to_owned()),
        "empty_dir" | "empty-dir" | "empty" => Ok("EMPTY_DIR".to_owned()),
        _ => Err(invalid_input(
            "--gdal-disable-readdir must be true, false, or empty-dir",
        )),
    }
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

fn parse_webp_quality(value: &str) -> io::Result<f32> {
    let quality = value
        .parse()
        .map_err(|_| invalid_input("--quality must be a number from 0 to 100"))?;
    if !(0.0..=100.0).contains(&quality) {
        return Err(invalid_input("--quality must be between 0 and 100"));
    }
    Ok(quality)
}

fn parse_webp_method(value: &str) -> io::Result<i32> {
    let method = value
        .parse()
        .map_err(|_| invalid_input("--webp-method must be an integer from 0 to 6"))?;
    if !(0..=6).contains(&method) {
        return Err(invalid_input("--webp-method must be between 0 and 6"));
    }
    Ok(method)
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

fn print_plan(opts: &RasterOptions, jobs: &[TileJob]) -> io::Result<()> {
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
    if opts.format == RasterFormat::Webp {
        print_field(
            "WebP",
            &format!(
                "quality {:.1}, method {}",
                opts.webp_quality, opts.webp_method
            ),
        );
    }
    if !opts.warp_options.is_empty() {
        println!();
        println!("Warp Options");
        for (name, value) in &opts.warp_options {
            print_field(name, value);
        }
    }
    println!();
    println!("Coverage");
    print_field("Chunk tiles", &opts.chunk_tiles.label());
    if let Some(cache_bytes) = opts.gdal_tuning.cache_bytes {
        print_field("GDAL cache", &format_bytes(cache_bytes));
    }
    if !opts.gdal_tuning.open_options.is_empty() {
        print_field(
            "GDAL open options",
            &format_count(opts.gdal_tuning.open_options.len() as u64),
        );
    }
    if !opts.gdal_tuning.config_options.is_empty() {
        print_field(
            "GDAL config",
            &format_count(opts.gdal_tuning.config_options.len() as u64),
        );
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
    if opts.chunk_tiles == ChunkTiles::Auto {
        println!();
        println!("Auto Chunk Tiles");
        for zoom_jobs in jobs_by_zoom(jobs) {
            let z = zoom_jobs[0].z;
            let chunk_tiles = auto_chunk_tiles(opts, zoom_jobs);
            print_field(&format!("z{z}"), &chunk_tiles.to_string());
        }
    }
    Ok(())
}

fn render_native(opts: &RasterOptions, jobs: &[TileJob]) -> io::Result<()> {
    let source = open_raster_dataset(&opts.input, &opts.gdal_tuning)?;
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
    if opts.format == RasterFormat::Webp {
        status_field(
            "WebP",
            &format!(
                "quality {:.1}, method {}",
                opts.webp_quality, opts.webp_method
            ),
        );
    }
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
        let elapsed = match chunk_tiles_for_zoom(opts, zoom_jobs) {
            Some(chunk_tiles) => {
                render_zoom_chunked_stream(&source, opts, zoom_jobs, chunk_tiles, &mut writer)?
            }
            None => {
                let mut rendered_zoom = render_zoom_wide(&source, opts, zoom_jobs)?;
                rendered_zoom.tiles.sort_by_key(|tile| tile.tile_id);
                for rendered in rendered_zoom.tiles {
                    write_rendered_tile(&mut writer, &rendered)?;
                }
                rendered_zoom.elapsed
            }
        };
        completed += zoom_jobs.len();
        status_field(
            &format!("Zoom z{zoom}"),
            &format!(
                "done in {:.1}s ({} of {}, {:.0}%)",
                elapsed,
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

fn write_rendered_tile(
    writer: &mut pmtiles::ArchiveWriter,
    rendered: &RenderedTile,
) -> io::Result<()> {
    writer.add_tile(
        rendered.tile_id,
        rendered.data.len().try_into().map_err(|_| {
            invalid_input(format!(
                "encoded tile exceeds PMTiles entry limit: {}",
                rendered.tile_id
            ))
        })?,
    );
    writer.write_tile_data(&rendered.data)
}

fn reserved_directory_bytes(tile_count: usize) -> u64 {
    let estimate = (tile_count as u64).saturating_mul(24);
    estimate.clamp(MIN_RESERVED_DIRECTORY_BYTES, MAX_RESERVED_DIRECTORY_BYTES)
}

fn chunk_tiles_for_zoom(opts: &RasterOptions, zoom_jobs: &[TileJob]) -> Option<u32> {
    match opts.chunk_tiles {
        ChunkTiles::Disabled => None,
        ChunkTiles::Fixed(chunk_tiles) => Some(chunk_tiles),
        ChunkTiles::Auto => Some(auto_chunk_tiles(opts, zoom_jobs)),
    }
}

fn auto_chunk_tiles(opts: &RasterOptions, zoom_jobs: &[TileJob]) -> u32 {
    let tile_count = zoom_jobs.len().max(1);
    let worker_count = opts.workers.max(1).min(tile_count);
    let target_chunks = worker_count
        .saturating_mul(DEFAULT_AUTO_CHUNK_TARGETS_PER_WORKER)
        .clamp(1, tile_count);

    let min_x = zoom_jobs.iter().map(|job| job.x).min().unwrap_or(0);
    let max_x = zoom_jobs.iter().map(|job| job.x).max().unwrap_or(min_x);
    let min_y = zoom_jobs.iter().map(|job| job.y).min().unwrap_or(0);
    let max_y = zoom_jobs.iter().map(|job| job.y).max().unwrap_or(min_y);
    let width_tiles = max_x - min_x + 1;
    let height_tiles = max_y - min_y + 1;
    let memory_budget = (opts.warp_memory_bytes.round() as usize / 2)
        .clamp(AUTO_CHUNK_MIN_BYTES, AUTO_CHUNK_MAX_BYTES);

    let mut chunk_tiles = 4_u32;
    while chunk_count(width_tiles, height_tiles, chunk_tiles) > target_chunks {
        let next = chunk_tiles.saturating_mul(2);
        if next == chunk_tiles
            || estimate_chunk_bytes_for_span(opts, next, next) > memory_budget
            || next > width_tiles.max(height_tiles).max(1).next_power_of_two()
        {
            break;
        }
        chunk_tiles = next;
    }

    while chunk_tiles > 1
        && estimate_chunk_bytes_for_span(opts, chunk_tiles, chunk_tiles) > memory_budget
    {
        chunk_tiles /= 2;
    }

    chunk_tiles.max(1)
}

fn chunk_count(width_tiles: u32, height_tiles: u32, chunk_tiles: u32) -> usize {
    let chunk_tiles = chunk_tiles.max(1);
    width_tiles.div_ceil(chunk_tiles) as usize * height_tiles.div_ceil(chunk_tiles) as usize
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
    let dataset = build_tile_dataset(source, opts, zoom_jobs, 1)?;
    let tiles = render_chunk_tiles_parallel(opts, dataset, zoom_jobs)?;
    Ok(RenderedZoom {
        tiles,
        elapsed: started.elapsed().as_secs_f64(),
    })
}

#[allow(dead_code)]
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
            "rendering {} in {} chunks of up to {}x{} tiles ({} workers, {}, {})",
            format_tile_count(zoom_jobs.len()),
            format_count(chunks.len() as u64),
            chunk_tiles,
            chunk_tiles,
            worker_count,
            strategy.label(),
            format_bytes((estimated_chunk_mib * 1024.0 * 1024.0).round() as u64)
        ),
    );

    if worker_count == 1 || chunks.len() <= 1 {
        let worker_source = open_raster_dataset(&opts.input, &opts.gdal_tuning)?;
        let mut rendered_zoom = Vec::with_capacity(zoom_jobs.len());
        let mut chunk_opts = opts.clone();
        chunk_opts.workers = 1;
        for (chunk_index, chunk_jobs) in chunks.iter().enumerate() {
            let chunk_started = std::time::Instant::now();
            let chunk_dataset = build_tile_dataset(&worker_source, &chunk_opts, chunk_jobs, 1)?;
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
                        let worker_source = open_raster_dataset(&opts.input, &opts.gdal_tuning)
                            .map_err(|err| err.to_string())?;
                        let mut chunk_opts = opts.clone();
                        chunk_opts.workers = 1;

                        loop {
                            let chunk_index = next_index.fetch_add(1, Ordering::Relaxed);
                            if chunk_index >= chunks.len() {
                                break;
                            }
                            let chunk_jobs = &chunks[chunk_index];
                            let chunk_started = std::time::Instant::now();
                            let chunk_dataset = build_tile_dataset(
                                &worker_source,
                                &chunk_opts,
                                chunk_jobs,
                                worker_count,
                            )
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

fn render_zoom_chunked_stream(
    source: &gdal::Dataset,
    opts: &RasterOptions,
    zoom_jobs: &[TileJob],
    chunk_tiles: u32,
    writer: &mut pmtiles::ArchiveWriter,
) -> io::Result<f64> {
    let zoom = zoom_jobs[0].z;
    let started = std::time::Instant::now();
    let chunks = chunk_jobs(zoom_jobs, chunk_tiles);
    let worker_count = opts.workers.max(1).min(chunks.len().max(1));
    let strategy = select_render_strategy(source, opts)?;
    let estimated_chunk_mib = estimate_chunk_working_set_mib(opts, zoom_jobs, chunk_tiles);
    status_field(
        &format!("Zoom z{zoom}"),
        &format!(
            "rendering {} in {} chunks of up to {}x{} tiles ({} workers, {}, {})",
            format_tile_count(zoom_jobs.len()),
            format_count(chunks.len() as u64),
            chunk_tiles,
            chunk_tiles,
            worker_count,
            strategy.label(),
            format_bytes((estimated_chunk_mib * 1024.0 * 1024.0).round() as u64)
        ),
    );

    let mut pending_tiles = BTreeMap::new();
    let mut next_expected = 0_usize;
    let mut completed_chunks = 0_usize;
    let mut zoom_timing = StageTiming::default();

    if worker_count == 1 || chunks.len() <= 1 {
        let worker_source = open_raster_dataset(&opts.input, &opts.gdal_tuning)?;
        let mut chunk_opts = opts.clone();
        chunk_opts.workers = 1;
        for chunk_jobs in &chunks {
            let chunk_started = std::time::Instant::now();
            let build_started = std::time::Instant::now();
            let chunk_dataset = build_tile_dataset(&worker_source, &chunk_opts, chunk_jobs, 1)?;
            let mut chunk_timing = StageTiming {
                build_dataset: build_started.elapsed().as_secs_f64(),
                ..StageTiming::default()
            };
            let rendered_chunk = render_chunk_tiles_timed(&chunk_opts, &chunk_dataset, chunk_jobs)?;
            chunk_timing.add(&rendered_chunk.timing);
            zoom_timing.add(&chunk_timing);
            completed_chunks += 1;
            status_field(
                &format!("Zoom z{zoom}"),
                &format!(
                    "chunk {}/{} done in {:.1}s ({})",
                    completed_chunks,
                    chunks.len(),
                    chunk_started.elapsed().as_secs_f64(),
                    format_tile_count(rendered_chunk.tiles.len())
                ),
            );
            let write_started = std::time::Instant::now();
            write_ready_tiles(
                writer,
                zoom_jobs,
                &mut next_expected,
                &mut pending_tiles,
                rendered_chunk.tiles,
            )?;
            zoom_timing.write_tiles += write_started.elapsed().as_secs_f64();
        }
    } else {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::mpsc;

        let next_index = AtomicUsize::new(0);
        let (tx, rx) = mpsc::channel();

        std::thread::scope(|scope| {
            for _ in 0..worker_count {
                let tx = tx.clone();
                let next_index = &next_index;
                let chunks = &chunks;
                scope.spawn(move || {
                    let result: Result<(), String> = (|| {
                        let worker_source = open_raster_dataset(&opts.input, &opts.gdal_tuning)
                            .map_err(|err| err.to_string())?;
                        let mut chunk_opts = opts.clone();
                        chunk_opts.workers = 1;

                        loop {
                            let chunk_index = next_index.fetch_add(1, Ordering::Relaxed);
                            if chunk_index >= chunks.len() {
                                break;
                            }
                            let chunk_jobs = &chunks[chunk_index];
                            let chunk_started = std::time::Instant::now();
                            let build_started = std::time::Instant::now();
                            let chunk_dataset = build_tile_dataset(
                                &worker_source,
                                &chunk_opts,
                                chunk_jobs,
                                worker_count,
                            )
                            .map_err(|err| err.to_string())?;
                            let mut chunk_timing = StageTiming {
                                build_dataset: build_started.elapsed().as_secs_f64(),
                                ..StageTiming::default()
                            };
                            let rendered_chunk =
                                render_chunk_tiles_timed(&chunk_opts, &chunk_dataset, chunk_jobs)
                                    .map_err(|err| err.to_string())?;
                            chunk_timing.add(&rendered_chunk.timing);
                            let elapsed = chunk_started.elapsed().as_secs_f64();
                            if tx
                                .send(Ok((rendered_chunk.tiles, elapsed, chunk_timing)))
                                .is_err()
                            {
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

            for result in rx {
                let (rendered_chunk, elapsed, chunk_timing) = result.map_err(io::Error::other)?;
                zoom_timing.add(&chunk_timing);
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
                let write_started = std::time::Instant::now();
                write_ready_tiles(
                    writer,
                    zoom_jobs,
                    &mut next_expected,
                    &mut pending_tiles,
                    rendered_chunk,
                )?;
                zoom_timing.write_tiles += write_started.elapsed().as_secs_f64();
            }

            Ok::<(), io::Error>(())
        })?;

        if completed_chunks != chunks.len() {
            return Err(io::Error::other("not all chunk workers completed"));
        }
    }

    if next_expected != zoom_jobs.len() || !pending_tiles.is_empty() {
        return Err(io::Error::other("not all rendered tiles were written"));
    }

    let elapsed = started.elapsed().as_secs_f64();
    status_field(
        &format!("Zoom z{zoom} stages"),
        &format!(
            "build/read {:.1}s, tile-read {:.1}s, encode {:.1}s, write {:.1}s (worker-sum), wall {:.1}s",
            zoom_timing.build_dataset,
            zoom_timing.read_tile,
            zoom_timing.encode_tile,
            zoom_timing.write_tiles,
            elapsed
        ),
    );
    Ok(elapsed)
}

fn write_ready_tiles(
    writer: &mut pmtiles::ArchiveWriter,
    expected_jobs: &[TileJob],
    next_expected: &mut usize,
    pending_tiles: &mut BTreeMap<u64, RenderedTile>,
    rendered_tiles: Vec<RenderedTile>,
) -> io::Result<()> {
    for rendered in rendered_tiles {
        pending_tiles.insert(rendered.tile_id, rendered);
    }

    while *next_expected < expected_jobs.len() {
        let tile_id = expected_jobs[*next_expected].tile_id;
        let Some(rendered) = pending_tiles.remove(&tile_id) else {
            break;
        };
        write_rendered_tile(writer, &rendered)?;
        *next_expected += 1;
    }

    Ok(())
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
    let bytes = estimate_chunk_bytes_for_span(opts, max_chunk_width_tiles, max_chunk_height_tiles);
    bytes as f64 / 1024.0 / 1024.0
}

fn estimate_chunk_bytes_for_span(
    opts: &RasterOptions,
    width_tiles: u32,
    height_tiles: u32,
) -> usize {
    let width = width_tiles as usize * opts.tile_size as usize;
    let height = height_tiles as usize * opts.tile_size as usize;
    let assumed_bands = 4_usize;

    // Fast paths keep planar output plus an interleaved read buffer in memory.
    width
        .saturating_mul(height)
        .saturating_mul(assumed_bands)
        .saturating_mul(2)
}

fn render_chunk_tiles_timed(
    opts: &RasterOptions,
    chunk_dataset: &ChunkDataset,
    jobs: &[TileJob],
) -> io::Result<RenderedChunk> {
    let mut rendered = Vec::with_capacity(jobs.len());
    let mut timing = StageTiming::default();
    let mut scratch = TileScratch::new(opts.tile_size as usize);

    for job in jobs {
        let read_started = std::time::Instant::now();
        let pixels = match chunk_dataset {
            ChunkDataset::Native(chunk) => read_native_tile_pixels(opts, chunk, job)?,
            ChunkDataset::Gdal(chunk) => read_gdal_tile_pixels(opts, chunk, job, &mut scratch)?,
        };
        timing.read_tile += read_started.elapsed().as_secs_f64();

        let encode_started = std::time::Instant::now();
        let data = encode_tile_gdal(opts, job, &pixels)?;
        timing.encode_tile += encode_started.elapsed().as_secs_f64();
        rendered.push(RenderedTile {
            tile_id: job.tile_id,
            data,
        });
    }

    Ok(RenderedChunk {
        tiles: rendered,
        timing,
    })
}

fn render_chunk_tiles_parallel(
    opts: &RasterOptions,
    chunk_dataset: ChunkDataset,
    jobs: &[TileJob],
) -> io::Result<Vec<RenderedTile>> {
    match chunk_dataset {
        ChunkDataset::Native(chunk) => render_native_chunk_tiles_parallel(opts, &chunk, jobs),
        ChunkDataset::Gdal(chunk) => render_gdal_chunk_tiles_parallel(opts, chunk, jobs),
    }
}

fn render_native_chunk_tiles_parallel(
    opts: &RasterOptions,
    chunk: &NativeChunk,
    jobs: &[TileJob],
) -> io::Result<Vec<RenderedTile>> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    let worker_count = opts.workers.max(1).min(jobs.len().max(1));
    if worker_count == 1 || jobs.len() <= 1 {
        let mut rendered = Vec::with_capacity(jobs.len());
        for job in jobs {
            let pixels = read_native_tile_pixels(opts, chunk, job)?;
            let data = encode_tile_gdal(opts, job, &pixels)?;
            rendered.push(RenderedTile {
                tile_id: job.tile_id,
                data,
            });
        }
        return Ok(rendered);
    }

    let next_index = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel();

    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            let tx = tx.clone();
            let next_index = &next_index;
            scope.spawn(move || {
                loop {
                    let index = next_index.fetch_add(1, Ordering::Relaxed);
                    if index >= jobs.len() {
                        break;
                    }
                    let job = jobs[index];
                    let result: Result<RenderedTile, String> = (|| {
                        let pixels = read_native_tile_pixels(opts, chunk, &job)
                            .map_err(|err| err.to_string())?;
                        let data =
                            encode_tile_gdal(opts, &job, &pixels).map_err(|err| err.to_string())?;
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

fn render_gdal_chunk_tiles_parallel(
    opts: &RasterOptions,
    chunk: GdalChunk,
    jobs: &[TileJob],
) -> io::Result<Vec<RenderedTile>> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, mpsc};

    let worker_count = opts.workers.max(1).min(jobs.len().max(1));
    if worker_count == 1 || jobs.len() <= 1 {
        let mut rendered = Vec::with_capacity(jobs.len());
        let mut scratch = TileScratch::new(opts.tile_size as usize);
        for job in jobs {
            let pixels = read_gdal_tile_pixels(opts, &chunk, job, &mut scratch)?;
            let data = encode_tile_gdal(opts, job, &pixels)?;
            rendered.push(RenderedTile {
                tile_id: job.tile_id,
                data,
            });
        }
        return Ok(rendered);
    }

    let chunk = Arc::new(Mutex::new(chunk));
    let next_index = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel();

    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            let tx = tx.clone();
            let chunk = Arc::clone(&chunk);
            let next_index = &next_index;
            scope.spawn(move || {
                let mut scratch = TileScratch::new(opts.tile_size as usize);
                loop {
                    let index = next_index.fetch_add(1, Ordering::Relaxed);
                    if index >= jobs.len() {
                        break;
                    }
                    let job = jobs[index];
                    let result: Result<RenderedTile, String> = (|| {
                        let pixels = {
                            let chunk = chunk
                                .lock()
                                .map_err(|_| "chunk dataset lock poisoned".to_owned())?;
                            read_gdal_tile_pixels(opts, &chunk, &job, &mut scratch)
                                .map_err(|err| err.to_string())?
                        };
                        let data =
                            encode_tile_gdal(opts, &job, &pixels).map_err(|err| err.to_string())?;
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
    concurrent_warps: usize,
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

    reproject_with_options(source, &dataset, opts, concurrent_warps)?;

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
                    ResampledReadWindow {
                        src_xoff,
                        src_yoff,
                        src_xsize,
                        src_ysize,
                        dst_width,
                        dst_height,
                        band_count,
                        resampling: opts.resampling,
                    },
                    &mut band_map,
                    &mut interleaved,
                )?;

                copy_interleaved_window(
                    &interleaved,
                    &mut pixels,
                    InterleavedWindow {
                        width: dst_width,
                        height: dst_height,
                        band_count,
                    },
                    DestinationWindow {
                        width,
                        x: dst_xoff,
                        y: dst_yoff,
                    },
                );
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
    let source_xoff = x_samples
        .iter()
        .map(|sample| sample.lower)
        .min()
        .unwrap_or(0);
    let source_xend = x_samples
        .iter()
        .map(|sample| sample.upper)
        .max()
        .unwrap_or(0)
        + 1;
    let source_window_width = source_xend.saturating_sub(source_xoff).max(1);

    let mut pixels = vec![0_u8; width * height * band_count];
    let row_len = source_window_width * band_count;
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
                source_xoff,
                y_sample.lower,
                source_window_width,
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
                    source_xoff,
                    y_sample.upper,
                    source_window_width,
                    band_count,
                    &mut band_map,
                    &mut row_b,
                )?;
            }
            cached_b = Some(y_sample.upper);
        }

        for (x, x_sample) in x_samples.iter().enumerate() {
            let out_offset = (y * width + x) * band_count;
            let lower = x_sample.lower - source_xoff;
            let upper = x_sample.upper - source_xoff;
            for band_offset in 0..band_count {
                let top_left = row_a[lower * band_count + band_offset] as f64;
                let top_right = row_a[upper * band_count + band_offset] as f64;
                let bottom_left = row_b[lower * band_count + band_offset] as f64;
                let bottom_right = row_b[upper * band_count + band_offset] as f64;
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

struct InterleavedWindow {
    width: usize,
    height: usize,
    band_count: usize,
}

struct DestinationWindow {
    width: usize,
    x: usize,
    y: usize,
}

fn copy_interleaved_window(
    source: &[u8],
    destination: &mut [u8],
    source_window: InterleavedWindow,
    destination_window: DestinationWindow,
) {
    let row_bytes = source_window.width * source_window.band_count;
    for y in 0..source_window.height {
        let source_start = y * row_bytes;
        let destination_start = ((destination_window.y + y) * destination_window.width
            + destination_window.x)
            * source_window.band_count;
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
    x: usize,
    y: usize,
    width: usize,
    band_count: usize,
    band_map: &mut [i32],
    buffer: &mut [u8],
) -> io::Result<()> {
    read_interleaved_row_window(source, x, y, width, band_count, band_map, buffer)
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

struct ResampledReadWindow {
    src_xoff: f64,
    src_yoff: f64,
    src_xsize: f64,
    src_ysize: f64,
    dst_width: usize,
    dst_height: usize,
    band_count: usize,
    resampling: Resampling,
}

fn read_interleaved_window_resampled(
    source: &gdal::Dataset,
    window: ResampledReadWindow,
    band_map: &mut [i32],
    buffer: &mut [u8],
) -> io::Result<()> {
    let rio_resampling = match window.resampling {
        Resampling::Nearest => gdal_sys::GDALRIOResampleAlg::GRIORA_NearestNeighbour,
        _ => gdal_sys::GDALRIOResampleAlg::GRIORA_Bilinear,
    };
    let mut extra = gdal_sys::GDALRasterIOExtraArg {
        nVersion: 1,
        eResampleAlg: rio_resampling,
        pfnProgress: None,
        pProgressData: std::ptr::null_mut(),
        bFloatingPointWindowValidity: 1,
        dfXOff: window.src_xoff,
        dfYOff: window.src_yoff,
        dfXSize: window.src_xsize,
        dfYSize: window.src_ysize,
    };
    let int_xoff = window.src_xoff.floor().max(0.0) as i32;
    let int_yoff = window.src_yoff.floor().max(0.0) as i32;
    let int_xsize = window.src_xsize.ceil().max(1.0) as i32;
    let int_ysize = window.src_ysize.ceil().max(1.0) as i32;
    let result = unsafe {
        gdal_sys::GDALDatasetRasterIOEx(
            source.c_dataset(),
            gdal_sys::GDALRWFlag::GF_Read,
            int_xoff,
            int_yoff,
            int_xsize,
            int_ysize,
            buffer.as_mut_ptr() as *mut std::ffi::c_void,
            window
                .dst_width
                .try_into()
                .map_err(|_| invalid_input("destination width exceeds GDAL int range"))?,
            window
                .dst_height
                .try_into()
                .map_err(|_| invalid_input("destination height exceeds GDAL int range"))?,
            gdal_sys::GDALDataType::GDT_Byte,
            window
                .band_count
                .try_into()
                .map_err(|_| invalid_input("band count exceeds GDAL int range"))?,
            band_map.as_mut_ptr(),
            window
                .band_count
                .try_into()
                .map_err(|_| invalid_input("pixel stride exceeds GDAL range"))?,
            (window.dst_width * window.band_count)
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
    concurrent_warps: usize,
) -> io::Result<()> {
    let mut warp_options = CslStringList::new();
    warp_options
        .set_name_value(
            "NUM_THREADS",
            &opts.warp_threads.as_gdal_value(concurrent_warps),
        )
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
        (*raw_options).papszWarpOptions = warp_options.into_ptr();
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

fn read_native_tile_pixels<'a>(
    opts: &RasterOptions,
    chunk: &'a NativeChunk,
    job: &TileJob,
) -> io::Result<TilePixels<'a>> {
    let tile_size = opts.tile_size as usize;
    let xoff = ((job.x - chunk.min_x) * opts.tile_size) as usize;
    let yoff = ((job.y - chunk.min_y) * opts.tile_size) as usize;
    let row_stride = chunk.width * chunk.band_count;
    let row_bytes = tile_size * chunk.band_count;
    let start = (yoff * chunk.width + xoff) * chunk.band_count;
    let length = (tile_size - 1)
        .saturating_mul(row_stride)
        .saturating_add(row_bytes);
    let end = start.saturating_add(length);
    if end > chunk.pixels.len() {
        return Err(io::Error::other("native chunk tile view is out of bounds"));
    }
    Ok(TilePixels {
        tile_size,
        band_count: chunk.band_count,
        row_stride,
        pixels: &chunk.pixels[start..end],
    })
}

fn read_gdal_tile_pixels<'a>(
    opts: &RasterOptions,
    chunk: &GdalChunk,
    job: &TileJob,
    scratch: &'a mut TileScratch,
) -> io::Result<TilePixels<'a>> {
    let tile_size = opts.tile_size as usize;
    let xoff = ((job.x - chunk.min_x) * opts.tile_size) as isize;
    let yoff = ((job.y - chunk.min_y) * opts.tile_size) as isize;
    scratch.bands.clear();
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
        scratch.bands.push(data.data().to_vec());
    }
    interleave_bands_into(&scratch.bands, tile_size * tile_size, &mut scratch.pixels);
    Ok(TilePixels {
        tile_size,
        band_count: chunk.band_count,
        row_stride: tile_size * chunk.band_count,
        pixels: &scratch.pixels,
    })
}

fn encode_tile_gdal(
    opts: &RasterOptions,
    job: &TileJob,
    pixels: &TilePixels<'_>,
) -> io::Result<Vec<u8>> {
    use gdal::DriverManager;
    use gdal::raster::{Buffer, RasterCreationOptions};

    if opts.format == RasterFormat::Webp && matches!(pixels.band_count, 3 | 4) {
        return encode_tile_webp(opts, pixels);
    }

    let mem_driver = DriverManager::get_driver_by_name("MEM").map_err(gdal_to_io)?;
    let tile_ds = mem_driver
        .create_with_band_type::<u8, _>("", pixels.tile_size, pixels.tile_size, pixels.band_count)
        .map_err(gdal_to_io)?;

    for band_offset in 0..pixels.band_count {
        let band_index = band_offset + 1;
        let band_data = deinterleave_band(pixels, band_offset);
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
            .add_name_value("QUALITY", &format!("{:.0}", opts.webp_quality))
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

fn encode_tile_webp(opts: &RasterOptions, pixels: &TilePixels<'_>) -> io::Result<Vec<u8>> {
    let width = i32::try_from(pixels.tile_size)
        .map_err(|_| invalid_input("tile width exceeds libwebp int range"))?;
    let height = i32::try_from(pixels.tile_size)
        .map_err(|_| invalid_input("tile height exceeds libwebp int range"))?;
    let stride = i32::try_from(pixels.row_stride)
        .map_err(|_| invalid_input("tile stride exceeds libwebp int range"))?;

    let mut config = std::mem::MaybeUninit::<WebPConfig>::uninit();
    if unsafe {
        WebPConfigInitInternal(
            config.as_mut_ptr(),
            0,
            opts.webp_quality,
            WEBP_ENCODER_ABI_VERSION,
        )
    } == 0
    {
        return Err(io::Error::other("libwebp config init failed"));
    }
    let mut config = unsafe { config.assume_init() };
    config.method = opts.webp_method;
    if unsafe { WebPValidateConfig(&config) } == 0 {
        return Err(invalid_input("invalid libwebp encoder configuration"));
    }

    let mut picture = std::mem::MaybeUninit::<WebPPicture>::uninit();
    if unsafe { WebPPictureInitInternal(picture.as_mut_ptr(), WEBP_ENCODER_ABI_VERSION) } == 0 {
        return Err(io::Error::other("libwebp picture init failed"));
    }
    let mut picture = unsafe { picture.assume_init() };
    picture.width = width;
    picture.height = height;

    let imported = unsafe {
        if pixels.band_count == 4 {
            WebPPictureImportRGBA(&mut picture, pixels.pixels.as_ptr(), stride)
        } else {
            WebPPictureImportRGB(&mut picture, pixels.pixels.as_ptr(), stride)
        }
    };
    if imported == 0 {
        unsafe {
            WebPPictureFree(&mut picture);
        }
        return Err(io::Error::other("libwebp picture import failed"));
    }

    let mut writer = std::mem::MaybeUninit::<WebPMemoryWriter>::uninit();
    unsafe {
        WebPMemoryWriterInit(writer.as_mut_ptr());
    }
    let mut writer = unsafe { writer.assume_init() };
    picture.writer = Some(WebPMemoryWrite);
    picture.custom_ptr = (&mut writer as *mut WebPMemoryWriter).cast();

    let encoded_ok = unsafe { WebPEncode(&config, &mut picture) };
    if encoded_ok == 0 {
        unsafe {
            WebPMemoryWriterClear(&mut writer);
            WebPPictureFree(&mut picture);
        }
        return Err(io::Error::other("libwebp encode failed"));
    }

    let encoded = unsafe { std::slice::from_raw_parts(writer.mem, writer.size).to_vec() };
    unsafe {
        WebPMemoryWriterClear(&mut writer);
        WebPPictureFree(&mut picture);
    }
    Ok(encoded)
}

fn interleave_bands_into(bands: &[Vec<u8>], pixel_count: usize, pixels: &mut Vec<u8>) {
    let band_count = bands.len();
    pixels.resize(pixel_count * band_count, 0);
    for pixel_index in 0..pixel_count {
        for band_offset in 0..band_count {
            pixels[pixel_index * band_count + band_offset] = bands[band_offset][pixel_index];
        }
    }
}

fn deinterleave_band(pixels: &TilePixels<'_>, band_offset: usize) -> Vec<u8> {
    let mut band = Vec::with_capacity(pixels.tile_size * pixels.tile_size);
    let row_bytes = pixels.tile_size * pixels.band_count;
    for y in 0..pixels.tile_size {
        let row_start = y * pixels.row_stride;
        let row = &pixels.pixels[row_start..row_start + row_bytes];
        band.extend(
            row.chunks_exact(pixels.band_count)
                .map(|pixel| pixel[band_offset]),
        );
    }
    band
}

struct RenderedTile {
    tile_id: u64,
    data: Vec<u8>,
}

struct RenderedChunk {
    tiles: Vec<RenderedTile>,
    timing: StageTiming,
}

struct RenderedZoom {
    tiles: Vec<RenderedTile>,
    elapsed: f64,
}

#[derive(Clone, Copy, Default)]
struct StageTiming {
    build_dataset: f64,
    read_tile: f64,
    encode_tile: f64,
    write_tiles: f64,
}

impl StageTiming {
    fn add(&mut self, other: &Self) {
        self.build_dataset += other.build_dataset;
        self.read_tile += other.read_tile;
        self.encode_tile += other.encode_tile;
        self.write_tiles += other.write_tiles;
    }
}

struct TileScratch {
    pixels: Vec<u8>,
    bands: Vec<Vec<u8>>,
}

impl TileScratch {
    fn new(tile_size: usize) -> Self {
        Self {
            pixels: Vec::with_capacity(tile_size * tile_size * 4),
            bands: Vec::new(),
        }
    }
}

struct TilePixels<'a> {
    tile_size: usize,
    band_count: usize,
    row_stride: usize,
    pixels: &'a [u8],
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
  --chunk-tiles <auto|n|off>
                         Chunk width/height in tiles, adaptive, or disabled/off [default: auto]
  --quality <0-100>      JPEG/WebP quality [default: 100]
  --webp-method <0-6>    WebP speed/size tradeoff, 0 fastest, 6 smallest [default: 4]
  --warp-memory <size>   GDAL warp memory, suffix K/M/G allowed [default: 512M]
  --warp-threads <auto|n|all>
                         GDAL warp compute threads [default: auto]
  --resampling <method>  nearest, bilinear, cubic, cubicspline, lanczos, average [default: bilinear]
  --strategy <strategy>  auto, same-crs, geographic, or gdal-warp [default: auto]
  --warp-option <K=V>    Extra GDAL warp option, repeatable
  --gdal-cache <size>    GDAL block cache size, suffix K/M/G allowed
  --gdal-config <K=V>    GDAL config option set before opening inputs, repeatable
  --open-option <K=V>    GDAL dataset open option, repeatable
  --gdal-disable-readdir <true|false|empty-dir>
                         Set GDAL_DISABLE_READDIR_ON_OPEN
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
    fn chunk_parser_accepts_auto_fixed_and_disabled() {
        assert_eq!(parse_chunk_tiles("auto").unwrap(), ChunkTiles::Auto);
        assert_eq!(parse_chunk_tiles("4").unwrap(), ChunkTiles::Fixed(4));
        assert_eq!(parse_chunk_tiles("off").unwrap(), ChunkTiles::Disabled);
    }

    #[test]
    fn auto_chunking_keeps_parallel_chunks_for_world_zoom_five() {
        let jobs =
            enumerate_tile_jobs([-180.0, -85.051_128_78, 180.0, 85.051_128_78], 5, 5).unwrap();
        let opts = RasterOptions {
            input: PathBuf::from("input.tif"),
            output: PathBuf::from("output.pmtiles"),
            min_zoom: 5,
            max_zoom: 5,
            bounds: [-180.0, -85.051_128_78, 180.0, 85.051_128_78],
            format: RasterFormat::Webp,
            tile_size: 512,
            workers: 8,
            chunk_tiles: ChunkTiles::Auto,
            warp_memory_bytes: 512.0 * 1024.0 * 1024.0,
            warp_threads: WarpThreads::Auto,
            resampling: Resampling::Bilinear,
            strategy: StrategyPreference::Auto,
            webp_quality: DEFAULT_WEBP_QUALITY,
            webp_method: DEFAULT_WEBP_METHOD,
            gdal_tuning: GdalTuning::default(),
            warp_options: Vec::new(),
            plan_only: false,
        };
        assert_eq!(auto_chunk_tiles(&opts, &jobs), 4);
    }

    #[test]
    fn auto_chunking_avoids_tiny_low_zoom_chunks() {
        let jobs =
            enumerate_tile_jobs([-180.0, -85.051_128_78, 180.0, 85.051_128_78], 2, 2).unwrap();
        let opts = RasterOptions {
            input: PathBuf::from("input.tif"),
            output: PathBuf::from("output.pmtiles"),
            min_zoom: 2,
            max_zoom: 2,
            bounds: [-180.0, -85.051_128_78, 180.0, 85.051_128_78],
            format: RasterFormat::Webp,
            tile_size: 512,
            workers: 8,
            chunk_tiles: ChunkTiles::Auto,
            warp_memory_bytes: 512.0 * 1024.0 * 1024.0,
            warp_threads: WarpThreads::Auto,
            resampling: Resampling::Bilinear,
            strategy: StrategyPreference::Auto,
            webp_quality: DEFAULT_WEBP_QUALITY,
            webp_method: DEFAULT_WEBP_METHOD,
            gdal_tuning: GdalTuning::default(),
            warp_options: Vec::new(),
            plan_only: false,
        };
        assert_eq!(auto_chunk_tiles(&opts, &jobs), 4);
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
