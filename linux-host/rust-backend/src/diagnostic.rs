//! Explicit opt-in observer. The caller enforces the 300-frame/15-second run limit,
//! Q85/420 socket-direct configuration, and reaps/releases Source before flush().
//! Nothing except a fresh private directory is written before flush().
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    ffi::CString,
    fs::File,
    io::Write,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Component, Path},
    sync::Arc,
    time::Instant,
};

const WIDTH: u32 = 2360;
const HEIGHT: u32 = 1640;
const ROI_WIDTH: u32 = 512;
const ROI_HEIGHT: u32 = 256;
const REGIONS: [(u32, u32); 2] = [(448, 480), (448, 960)];
const MAX_JPEG: usize = 8 * 1024 * 1024;
const MAX_PIXELS: usize = 8_388_608;
const RAW_LEN: usize = WIDTH as usize * HEIGHT as usize * 4;

/// Owns only the before-hash and two RGB crops, never a full raw frame.
/// Pass directly to after(); do not retain it or call before() twice in a row.
pub struct Snapshot {
    owner: Arc<()>,
    raw_before_sha256: String,
    crops: [Vec<u8>; 2],
    before_ms: f64,
}

struct Sample {
    frame: u32,
    snapshot: Snapshot,
    raw_after_sha256: String,
    jpeg_sha256: String,
    jpeg: Vec<u8>,
    after_ms: f64,
    inline_span_ms: f64,
}

#[derive(Default)]
struct State {
    pending: Vec<Sample>,
    in_flight: bool,
    closed: bool,
}
impl State {
    fn abort(&mut self) {
        self.pending.clear();
        self.in_flight = false;
        self.closed = true;
    }
}

/// No environment lookup or automatic enablement: construct only on explicit opt-in.
/// Errors permanently disable this instance and discard pending JPEGs/crops.
pub struct Diagnostic {
    directory: File,
    owner: Arc<()>,
    state: RefCell<State>,
}

impl Diagnostic {
    pub fn new(directory: &Path, width: u32, height: u32) -> Result<Self> {
        // Must precede *any* filesystem effect, including directory creation.
        ensure!(
            (width, height) == (WIDTH, HEIGHT),
            "diagnostic requires native 2360x1640 geometry"
        );
        Ok(Self {
            directory: fresh_directory(directory)?,
            owner: Arc::new(()),
            state: RefCell::new(State::default()),
        })
    }

    pub fn selected(frame: u32) -> bool {
        matches!(frame, 90 | 180 | 270)
    }

    /// Input is exactly packed native BGRX, fully filled by the single-owner source.
    pub fn before(&self, raw: &[u8]) -> Result<Snapshot> {
        let started = Instant::now();
        let mut state = self.state.borrow_mut();
        let result = (|| {
            ensure!(!state.closed, "diagnostic already closed or failed");
            ensure!(!state.in_flight, "diagnostic snapshot already outstanding");
            ensure!(
                state.pending.len() < 3,
                "diagnostic three-sample limit reached"
            );
            ensure!(raw.len() == RAW_LEN, "diagnostic raw length mismatch");
            let raw_before_sha256 = digest(raw);
            let crops = crops(raw, 4);
            state.in_flight = true;
            Ok(Snapshot {
                owner: Arc::clone(&self.owner),
                raw_before_sha256,
                crops,
                before_ms: started.elapsed().as_secs_f64() * 1000.0,
            })
        })();
        if result.is_err() {
            state.abort();
        }
        result
    }

    /// Copies immutable encoded bytes before the Source output mapping is reused.
    /// Observer timing is before_ms + after_ms; the separate span includes encoding.
    pub fn after(
        &mut self,
        frame: u32,
        snapshot: Snapshot,
        raw: &[u8],
        jpeg: &[u8],
        inline_started: Instant,
    ) -> Result<()> {
        let started = Instant::now();
        let mut state = self.state.borrow_mut();
        let result = (|| {
            ensure!(!state.closed, "diagnostic already closed or failed");
            ensure!(
                state.in_flight && Arc::ptr_eq(&snapshot.owner, &self.owner),
                "diagnostic snapshot owner/state mismatch"
            );
            ensure!(Self::selected(frame), "diagnostic frame is not selected");
            ensure!(
                state.pending.len() < 3,
                "diagnostic three-sample limit reached"
            );
            ensure!(
                !state.pending.iter().any(|s| s.frame == frame),
                "duplicate diagnostic frame"
            );
            ensure!(raw.len() == RAW_LEN, "diagnostic raw length mismatch");
            ensure!(
                !jpeg.is_empty() && jpeg.len() <= MAX_JPEG,
                "diagnostic JPEG must be 1..=8 MiB"
            );
            let raw_after_sha256 = digest(raw);
            let jpeg_sha256 = digest(jpeg);
            let jpeg = jpeg.to_vec();
            state.pending.push(Sample {
                frame,
                snapshot,
                raw_after_sha256,
                jpeg_sha256,
                jpeg,
                after_ms: 0.0,
                inline_span_ms: 0.0,
            });
            let sample = state.pending.last_mut().unwrap();
            sample.after_ms = started.elapsed().as_secs_f64() * 1000.0;
            sample.inline_span_ms = inline_started.elapsed().as_secs_f64() * 1000.0;
            state.in_flight = false;
            Ok(())
        })();
        if result.is_err() {
            state.abort();
        }
        result
    }

    /// Call ONLY AFTER all Source children have been reaped and mappings released.
    /// One-shot even on failure. Partial files remain as evidence; never retry flush.
    pub fn flush(&mut self) -> Result<()> {
        let pending = {
            let mut state = self.state.borrow_mut();
            if state.closed || state.in_flight {
                state.abort();
                anyhow::bail!("diagnostic cannot flush: closed/failed or snapshot outstanding");
            }
            state.closed = true;
            std::mem::take(&mut state.pending)
        };
        // All pending memory is owned by this scope, so every error drops it.
        let mut log = private_file(&self.directory, "samples.jsonl")?;
        for sample in pending {
            let started = Instant::now();
            let decoded = decode(&sample.jpeg)?;
            let decoded_crops = crops(&decoded, 3);
            let mut regions = Vec::with_capacity(2);
            for (index, &(x, y)) in REGIONS.iter().enumerate() {
                let raw_path = format!("frame-{}-roi-{}-raw.png", sample.frame, index);
                let decoded_path = format!("frame-{}-roi-{}-decoded.png", sample.frame, index);
                save_png(
                    private_file(&self.directory, &raw_path)?,
                    &sample.snapshot.crops[index],
                )?;
                save_png(
                    private_file(&self.directory, &decoded_path)?,
                    &decoded_crops[index],
                )?;
                regions.push(serde_json::json!({
                    "x": x, "y": y, "width": ROI_WIDTH, "height": ROI_HEIGHT,
                    "raw_png": raw_path, "decoded_png": decoded_path,
                }));
            }
            let deferred_ms = started.elapsed().as_secs_f64() * 1000.0;
            let record = serde_json::json!({
                "frame": sample.frame,
                "shape": {"width": WIDTH, "height": HEIGHT, "raw_format": "BGRX", "crop_format": "RGB"},
                "regions": regions,
                "raw_before_sha256": sample.snapshot.raw_before_sha256,
                "raw_after_sha256": sample.raw_after_sha256,
                "raw_stable": sample.snapshot.raw_before_sha256 == sample.raw_after_sha256,
                "jpeg_sha256": sample.jpeg_sha256,
                "before_ms": sample.snapshot.before_ms, "after_ms": sample.after_ms,
                "inline_observer_ms": sample.snapshot.before_ms + sample.after_ms,
                "inline_observer_timing": "before_ms+after_ms; excludes encode and caller work",
                "inline_span_including_encode_ms": sample.inline_span_ms,
                "deferred_ms": deferred_ms,
                "deferred_timing": "decode, RGB crop and PNG writes; excludes this JSONL write",
                "receiver_verified": false, "root_cause_verified": false,
            });
            serde_json::to_writer(&mut log, &record)?;
            log.write_all(b"\n")?;
        }
        log.flush()?;
        Ok(())
    }
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn crops(raw: &[u8], channels: usize) -> [Vec<u8>; 2] {
    REGIONS.map(|(x, y)| {
        let mut rgb = Vec::with_capacity((ROI_WIDTH * ROI_HEIGHT * 3) as usize);
        for row in y..y + ROI_HEIGHT {
            let start = (row as usize * WIDTH as usize + x as usize) * channels;
            for pixel in raw[start..start + ROI_WIDTH as usize * channels].chunks_exact(channels) {
                if channels == 4 {
                    rgb.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
                } else {
                    rgb.extend_from_slice(pixel);
                }
            }
        }
        rgb
    })
}

// Walk directory components with no-follow handles. All output is relative to the
// held directory FD, not a path that could subsequently resolve through a symlink.
fn open_directory(parent: libc::c_int, name: &std::ffi::CStr) -> Result<File> {
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    ensure!(
        fd >= 0,
        "open diagnostic directory: {}",
        std::io::Error::last_os_error()
    );
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn fresh_directory(path: &Path) -> Result<File> {
    let components: Vec<_> = path.components().collect();
    ensure!(!components.is_empty(), "empty diagnostic directory");
    let mut parent = open_directory(libc::AT_FDCWD, if path.is_absolute() { c"/" } else { c"." })?;
    let mut names = Vec::new();
    for component in components {
        match component {
            Component::RootDir | Component::CurDir => (),
            Component::Normal(name) => names.push(CString::new(name.as_bytes())?),
            _ => anyhow::bail!("diagnostic path must not contain parent/prefix components"),
        }
    }
    let leaf = names
        .pop()
        .context("diagnostic requires a fresh directory name")?;
    for name in names {
        parent = open_directory(parent.as_raw_fd(), &name)?;
    }
    let rc = unsafe { libc::mkdirat(parent.as_raw_fd(), leaf.as_ptr(), 0o700) };
    ensure!(
        rc == 0,
        "create fresh diagnostic directory: {}",
        std::io::Error::last_os_error()
    );
    let directory = open_directory(parent.as_raw_fd(), &leaf)?;
    let metadata = directory.metadata()?;
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() },
        "diagnostic directory is not owned by current user"
    );
    ensure!(
        unsafe { libc::fchmod(directory.as_raw_fd(), 0o700) } == 0,
        "chmod diagnostic directory: {}",
        std::io::Error::last_os_error()
    );
    Ok(directory)
}

fn private_file(directory: &File, name: &str) -> Result<File> {
    ensure!(
        !name.is_empty() && !name.contains('/') && name != "." && name != "..",
        "invalid diagnostic filename"
    );
    let name = CString::new(name)?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    ensure!(
        fd >= 0,
        "create diagnostic file: {}",
        std::io::Error::last_os_error()
    );
    let file = unsafe { File::from_raw_fd(fd) };
    ensure!(
        unsafe { libc::fchmod(fd, 0o600) } == 0,
        "chmod diagnostic file: {}",
        std::io::Error::last_os_error()
    );
    Ok(file)
}

fn save_png(file: File, rgb: &[u8]) -> Result<()> {
    let mut encoder = png::Encoder::new(file, ROI_WIDTH, ROI_HEIGHT);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(rgb)?;
    writer.finish()?;
    Ok(())
}

#[link(name = "turbojpeg")]
unsafe extern "C" {
    fn tjInitDecompress() -> *mut libc::c_void;
    fn tjDecompressHeader3(
        handle: *mut libc::c_void,
        jpeg: *const u8,
        len: libc::c_ulong,
        width: *mut libc::c_int,
        height: *mut libc::c_int,
        subsamp: *mut libc::c_int,
        colorspace: *mut libc::c_int,
    ) -> libc::c_int;
    fn tjDecompress2(
        handle: *mut libc::c_void,
        jpeg: *const u8,
        len: libc::c_ulong,
        output: *mut u8,
        width: libc::c_int,
        pitch: libc::c_int,
        height: libc::c_int,
        format: libc::c_int,
        flags: libc::c_int,
    ) -> libc::c_int;
    fn tjDestroy(handle: *mut libc::c_void) -> libc::c_int;
}
struct Decoder(*mut libc::c_void);
impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            tjDestroy(self.0);
        }
    }
}
fn decode(jpeg: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        !jpeg.is_empty() && jpeg.len() <= MAX_JPEG,
        "invalid retained JPEG size"
    );
    let handle = unsafe { tjInitDecompress() };
    ensure!(!handle.is_null(), "tjInitDecompress failed");
    let decoder = Decoder(handle);
    let (mut width, mut height, mut subsamp, mut colorspace) = (0, 0, 0, 0);
    let len = libc::c_ulong::try_from(jpeg.len())?;
    ensure!(
        unsafe {
            tjDecompressHeader3(
                decoder.0,
                jpeg.as_ptr(),
                len,
                &mut width,
                &mut height,
                &mut subsamp,
                &mut colorspace,
            )
        } == 0,
        "tjDecompressHeader3 failed"
    );
    ensure!(
        (width, height) == (WIDTH as i32, HEIGHT as i32),
        "JPEG header does not have native 2360x1640 geometry"
    );
    let pixels = usize::try_from(width)?
        .checked_mul(usize::try_from(height)?)
        .context("JPEG pixel count overflow")?;
    ensure!(pixels <= MAX_PIXELS, "JPEG pixel allocation exceeds limit");
    let bytes = pixels
        .checked_mul(3)
        .context("JPEG RGB allocation overflow")?;
    let mut rgb = vec![0; bytes];
    ensure!(
        unsafe {
            tjDecompress2(
                decoder.0,
                jpeg.as_ptr(),
                len,
                rgb.as_mut_ptr(),
                width,
                width * 3,
                height,
                0,
                0,
            )
        } == 0,
        "tjDecompress2 RGB decode failed"
    );
    Ok(rgb)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    // No extra tempdir dependency; this guard removes only its uniquely created,
    // verified owned temp directory. It never points at caller-selected paths.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "xdisplay-diagnostic-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }
        fn output(&self) -> PathBuf {
            self.0.join("output")
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let metadata = fs::symlink_metadata(&self.0).unwrap();
            assert!(metadata.is_dir() && metadata.uid() == unsafe { libc::geteuid() });
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    use std::os::unix::fs::DirBuilderExt;

    #[test]
    fn wrong_geometry_creates_nothing() {
        let temp = TempDir::new();
        assert!(Diagnostic::new(&temp.output(), 1920, 1080).is_err());
        assert!(!temp.output().exists());
    }

    #[test]
    fn selection_raw_size_and_private_fresh_paths() {
        assert!([90, 180, 270].into_iter().all(Diagnostic::selected));
        assert!(
            [0, 89, 91, 300]
                .into_iter()
                .all(|f| !Diagnostic::selected(f))
        );
        let temp = TempDir::new();
        let diagnostic = Diagnostic::new(&temp.output(), WIDTH, HEIGHT).unwrap();
        assert!(Diagnostic::new(&temp.output(), WIDTH, HEIGHT).is_err());
        assert!(diagnostic.before(&[0; 4]).is_err());
        assert!(fs::read_dir(temp.output()).unwrap().next().is_none());
        let link = temp.0.join("link");
        std::os::unix::fs::symlink(temp.output(), &link).unwrap();
        assert!(Diagnostic::new(&link, WIDTH, HEIGHT).is_err());
        assert!(Diagnostic::new(&link.join("nested"), WIDTH, HEIGHT).is_err());
        assert!(!temp.output().join("nested").exists());
    }

    #[test]
    fn crops_are_owned_rgb_and_errors_clear_pending() {
        let temp = TempDir::new();
        let mut diagnostic = Diagnostic::new(&temp.output(), WIDTH, HEIGHT).unwrap();
        let mut raw = vec![0; RAW_LEN];
        let first = (480 * WIDTH as usize + 448) * 4;
        raw[first..first + 4].copy_from_slice(&[1, 2, 3, 255]);
        let snapshot = diagnostic.before(&raw).unwrap();
        raw.fill(9);
        assert_eq!(&snapshot.crops[0][..3], &[3, 2, 1]);
        diagnostic
            .after(90, snapshot, &raw, &[1], Instant::now())
            .unwrap();
        assert_eq!(fs::read_dir(temp.output()).unwrap().count(), 0);
        let snapshot = diagnostic.before(&raw).unwrap();
        assert!(
            diagnostic
                .after(180, snapshot, &raw, &vec![0; MAX_JPEG + 1], Instant::now())
                .is_err()
        );
        assert!(diagnostic.state.borrow().pending.is_empty());
        assert!(diagnostic.flush().is_err());
    }

    #[test]
    fn three_sample_guard_drops_memory() {
        let temp = TempDir::new();
        let mut diagnostic = Diagnostic::new(&temp.output(), WIDTH, HEIGHT).unwrap();
        let raw = vec![0; RAW_LEN];
        for frame in [90, 180, 270] {
            let snapshot = diagnostic.before(&raw).unwrap();
            diagnostic
                .after(frame, snapshot, &raw, &[1], Instant::now())
                .unwrap();
        }
        assert_eq!(diagnostic.state.borrow().pending.len(), 3);
        assert!(diagnostic.before(&raw).is_err());
        assert!(diagnostic.state.borrow().pending.is_empty());
    }

    #[test]
    fn flush_failure_cannot_retry_or_overwrite() {
        let temp = TempDir::new();
        let mut diagnostic = Diagnostic::new(&temp.output(), WIDTH, HEIGHT).unwrap();
        let raw = vec![0; RAW_LEN];
        let snapshot = diagnostic.before(&raw).unwrap();
        diagnostic
            .after(90, snapshot, &raw, &[1], Instant::now())
            .unwrap();
        fs::write(temp.output().join("samples.jsonl"), b"untouched").unwrap();
        assert!(diagnostic.flush().is_err());
        assert!(diagnostic.flush().is_err());
        assert!(diagnostic.state.borrow().pending.is_empty());
        assert_eq!(
            fs::read(temp.output().join("samples.jsonl")).unwrap(),
            b"untouched"
        );
    }

    #[link(name = "turbojpeg")]
    unsafe extern "C" {
        fn tjInitCompress() -> *mut libc::c_void;
        fn tjCompress2(
            handle: *mut libc::c_void,
            raw: *const u8,
            width: libc::c_int,
            pitch: libc::c_int,
            height: libc::c_int,
            format: libc::c_int,
            jpeg: *mut *mut u8,
            len: *mut libc::c_ulong,
            subsamp: libc::c_int,
            quality: libc::c_int,
            flags: libc::c_int,
        ) -> libc::c_int;
        fn tjFree(buffer: *mut u8);
    }
    fn native_jpeg(raw: &[u8], width: u32, height: u32) -> Vec<u8> {
        let encoder = Decoder(unsafe { tjInitCompress() });
        assert!(!encoder.0.is_null());
        let (mut output, mut len) = (std::ptr::null_mut(), 0);
        let rc = unsafe {
            tjCompress2(
                encoder.0,
                raw.as_ptr(),
                width as i32,
                width as i32 * 4,
                height as i32,
                3,
                &mut output,
                &mut len,
                2,
                85,
                0,
            )
        };
        if rc != 0 {
            if !output.is_null() {
                unsafe {
                    tjFree(output);
                }
            }
            panic!("native JPEG test encoding failed");
        }
        let jpeg = unsafe { std::slice::from_raw_parts(output, len as usize) }.to_vec();
        unsafe {
            tjFree(output);
        }
        jpeg
    }

    #[test]
    fn native_jpeg_to_twelve_roi_pngs() {
        let temp = TempDir::new();
        let mut diagnostic = Diagnostic::new(&temp.output(), WIDTH, HEIGHT).unwrap();
        let raw = [16, 64, 128, 0].repeat(RAW_LEN / 4);
        let jpeg = native_jpeg(&raw, WIDTH, HEIGHT);
        let rgb = decode(&jpeg).unwrap();
        assert_eq!(rgb.len(), WIDTH as usize * HEIGHT as usize * 3);
        assert!((rgb[0] as i16 - 128).abs() <= 3);
        assert!(decode(&native_jpeg(&[0; 16 * 16 * 4], 16, 16)).is_err());
        for frame in [90, 180, 270] {
            let started = Instant::now();
            let snapshot = diagnostic.before(&raw).unwrap();
            diagnostic
                .after(frame, snapshot, &raw, &jpeg, started)
                .unwrap();
        }
        assert_eq!(fs::read_dir(temp.output()).unwrap().count(), 0);
        diagnostic.flush().unwrap();
        assert!(diagnostic.flush().is_err());
        let files: Vec<_> = fs::read_dir(temp.output())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(files.len(), 13);
        assert_eq!(
            fs::metadata(temp.output()).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let mut png_count = 0;
        for path in files {
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            if path.extension().is_some_and(|e| e == "png") {
                png_count += 1;
                let reader = png::Decoder::new(std::io::BufReader::new(File::open(path).unwrap()))
                    .read_info()
                    .unwrap();
                assert_eq!(
                    (reader.info().width, reader.info().height),
                    (ROI_WIDTH, ROI_HEIGHT)
                );
                assert_eq!(reader.info().color_type, png::ColorType::Rgb);
            }
        }
        assert_eq!(png_count, 12);
        let records = fs::read_to_string(temp.output().join("samples.jsonl")).unwrap();
        assert_eq!(records.lines().count(), 3);
        for line in records.lines() {
            let record: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(record["raw_stable"], true);
            assert_eq!(record["receiver_verified"], false);
            assert_eq!(record["root_cause_verified"], false);
        }
    }
}
