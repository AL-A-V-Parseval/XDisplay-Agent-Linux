//! Single-owner native frame source. No Python, desktop portal or raw-slot sharing.
use crate::{
    Settings,
    process::{Stop, reap},
};
use anyhow::{Context, Result, bail, ensure};
use std::{
    fs::File,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::net::UnixStream as StdUnix,
    },
    path::Path,
    process::Stdio,
    ptr::NonNull,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    process::{Child, ChildStdin, ChildStdout, Command},
    time::timeout,
};
const MAX_PIXELS: usize = 8_388_608;
const JPEG_CAP: usize = 64 * 1024 * 1024;

struct Mapping {
    ptr: NonNull<u8>,
    len: usize,
    fd: File,
}
impl Mapping {
    fn new(size: usize, writable: bool) -> Result<Self> {
        ensure!(size > 0 && size <= JPEG_CAP, "invalid mapping size");
        let fd = unsafe {
            libc::memfd_create(
                c"xdisplay-rust-owned".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        ensure!(fd >= 0, "memfd_create failed");
        let file = unsafe { File::from_raw_fd(fd) };
        file.set_len(size as u64)?;
        let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;
        ensure!(
            unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, seals) } == 0,
            "memfd sealing failed"
        );
        let protection = libc::PROT_READ | if writable { libc::PROT_WRITE } else { 0 };
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                protection,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        ensure!(address != libc::MAP_FAILED, "mmap failed");
        Ok(Self {
            ptr: NonNull::new(address.cast()).context("null mmap")?,
            len: size,
            fd: file,
        })
    }
    fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
    fn bytes_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}
impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr.as_ptr().cast(), self.len);
        }
    }
}

#[derive(Default)]
pub(crate) struct SocketSourceTiming {
    pub header: Duration,
    pub allocate: Duration,
    pub receive: Duration,
    pub copy: Duration,
    pub encode: Duration,
}

pub struct Source {
    pub(crate) socket_timing: Option<SocketSourceTiming>,
    settings: Settings,
    width: u32,
    height: u32,
    root: std::path::PathBuf,
    helper: Option<Child>,
    codec: Option<Child>,
    request: Option<ChildStdin>,
    raw: Option<UnixStream>,
    codec_in: Option<ChildStdin>,
    codec_out: Option<ChildStdout>,
    staging: Vec<u8>,
    input: Option<Mapping>,
    output: Option<Mapping>,
    pub encoded: u32,
    pub encode_seconds: f64,
    failed: bool,
    #[cfg(feature = "frame-diagnostics")]
    diagnostic: Option<crate::diagnostic::Diagnostic>,
    #[cfg(feature = "frame-diagnostics")]
    stage_hits_emitted: u32,
}
impl Source {
    pub fn new(settings: &Settings, width: u32, height: u32) -> Result<Self> {
        validate_size(width, height)?;
        Ok(Self {
            settings: settings.clone(),
            socket_timing: None,
            width,
            height,
            root: std::path::PathBuf::new(),
            helper: None,
            codec: None,
            request: None,
            raw: None,
            codec_in: None,
            codec_out: None,
            staging: Vec::new(),
            input: Some(Mapping::new(width as usize * height as usize * 4, true)?),
            output: Some(Mapping::new(JPEG_CAP, false)?),
            encoded: 0,
            encode_seconds: 0.,
            failed: false,
            #[cfg(feature = "frame-diagnostics")]
            stage_hits_emitted: 0,
            #[cfg(feature = "frame-diagnostics")]
            diagnostic: settings
                .diagnostic_dir
                .as_deref()
                .map(|dir| crate::diagnostic::Diagnostic::new(dir, width, height))
                .transpose()?,
        })
    }
    pub async fn start(&mut self, root: &Path, stop: &Stop) -> Result<()> {
        stop.check()?;
        ensure!(
            self.helper.is_none() && !self.failed,
            "source already started/failed"
        );
        let (parent, peer) = StdUnix::pair()?;
        parent.set_nonblocking(true)?;
        self.raw = Some(UnixStream::from_std(parent)?);
        let helper_path = root.join("vkms-read");
        #[cfg(feature = "frame-diagnostics")]
        let helper_path = if self.diagnostic.is_some() {
            let path = std::env::current_exe()?.with_file_name("vkms-read-cursor-probe");
            ensure!(path.is_file(), "缺少诊断读取器 vkms-read-cursor-probe");
            path
        } else {
            helper_path
        };
        let mut command = if self.settings.elevate {
            let mut cmd = Command::new("/usr/bin/pkexec");
            cmd.arg(&helper_path);
            cmd
        } else {
            Command::new(&helper_path)
        };
        command.args([
            self.settings.device.clone(),
            self.settings.connector.to_string(),
            self.settings.frames.to_string(),
        ]);
        if self.settings.continuous {
            command.arg("--continuous");
        }
        #[cfg(feature = "frame-diagnostics")]
        if self.diagnostic.is_some() {
            command.arg("--cursor-stage-probe");
        }
        let helper = command
            .stdin(Stdio::piped())
            .stdout(Stdio::from(OwnedFd::from(peer)))
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("无法启动原生VKMS读取器")?;
        self.helper = Some(helper);
        self.request = self.helper.as_mut().unwrap().stdin.take();
        self.root = root.into();
        Ok(())
    }
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    fn start_codec(&mut self) -> Result<()> {
        let input_fd = self.input.as_ref().unwrap().fd.as_raw_fd();
        let output_fd = self.output.as_ref().unwrap().fd.as_raw_fd();
        let mut cmd = Command::new(self.root.join("turbo-jpeg"));
        cmd.args([
            "--shared-fd".to_string(),
            input_fd.to_string(),
            "--shared-output-fd".to_string(),
            output_fd.to_string(),
            "--quality".to_string(),
            self.settings.quality.to_string(),
        ]);
        if self.settings.continuous {
            cmd.arg("--continuous");
        }
        // Only these two owned memfds are inherited by this unprivileged pixel-only worker.
        unsafe {
            cmd.pre_exec(move || {
                for fd in [input_fd, output_fd] {
                    if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        self.codec = Some(
            cmd.stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .context("无法启动原生JPEG编码器")?,
        );
        let child = self.codec.as_mut().unwrap();
        self.codec_in = child.stdin.take();
        self.codec_out = child.stdout.take();
        Ok(())
    }
    pub async fn next(
        &mut self,
        width: u32,
        height: u32,
        duration: Duration,
        stop: &Stop,
    ) -> Result<Vec<u8>> {
        ensure!(
            !self.failed && (self.settings.continuous || self.encoded < self.settings.frames),
            "帧源已失败或达到帧数限制"
        );
        ensure!(
            (width, height) == (self.width, self.height),
            "协商画布与虚拟输出不同；不会隐式缩放"
        );
        self.socket_timing = None;
        // Ignore the prepared frame: it can include authorization and codec startup.
        let measure = self.settings.socket_timing && self.encoded > 0;
        let operation = async {
            let requested = measure.then(Instant::now);
            self.request
                .as_mut()
                .context("missing frame helper")?
                .write_all(b"F")
                .await?;
            let raw = self.raw.as_mut().context("missing raw socket")?;
            let mut header = [0; 16];
            raw.read_exact(&mut header).await?;
            validate_raw_header(&header, width, height)?;
            let mapping = self.input.as_mut().context("input released")?;
            let mut timing = requested.map(|start| SocketSourceTiming {
                header: start.elapsed(),
                ..Default::default()
            });
            if self.settings.transport == "socket-direct" {
                raw.read_exact(mapping.bytes_mut()).await?;
            } else {
                let allocating = measure.then(Instant::now);
                let mut bytes = if self.settings.socket_reuse_staging {
                    let mut bytes = std::mem::take(&mut self.staging);
                    bytes.resize(mapping.len, 0);
                    bytes
                } else {
                    vec![0; mapping.len]
                };
                let receiving = timing.as_mut().map(|sample| {
                    sample.allocate = allocating.unwrap().elapsed();
                    Instant::now()
                });
                raw.read_exact(&mut bytes).await?;
                let copying = timing.as_mut().map(|sample| {
                    sample.receive = receiving.unwrap().elapsed();
                    Instant::now()
                });
                mapping.bytes_mut().copy_from_slice(&bytes);
                if self.settings.socket_reuse_staging {
                    self.staging = bytes;
                } else {
                    drop(bytes);
                }
                if let Some(sample) = &mut timing {
                    sample.copy = copying.unwrap().elapsed();
                }
            }
            #[cfg(feature = "frame-diagnostics")]
            if self.diagnostic.is_some() {
                let mut record = [0; 92];
                raw.read_exact(&mut record).await?;
                let event = cursor_stage_event(&record, self.encoded + 1)?;
                let hit = event["known_coordinate_hit"].as_bool() == Some(true)
                    || !event["roi"]["present_before"].is_null()
                    || !event["roi"]["new_after"].is_null();
                if matches!(self.encoded + 1, 1 | 90 | 180 | 270)
                    || (hit && self.stage_hits_emitted < 12)
                {
                    crate::emit(event);
                    if hit {
                        self.stage_hits_emitted += 1;
                    }
                }
            }
            // Do not start the codec's bounded stdin clock while root authorization is pending.
            if self.codec.is_none() {
                self.start_codec()?;
            }
            // Full immutable helper snapshot received before requesting compression.
            #[cfg(feature = "frame-diagnostics")]
            let snapshot = match &self.diagnostic {
                Some(probe) if crate::diagnostic::Diagnostic::selected(self.encoded + 1) => {
                    let started = Instant::now();
                    Some((probe.before(self.input.as_ref().unwrap().bytes())?, started))
                }
                _ => None,
            };
            let began = Instant::now();
            let mut command = Vec::from(*b"BGR1");
            for word in [width, height, width * height * 4] {
                command.extend(word.to_le_bytes());
            }
            self.codec_in
                .as_mut()
                .context("missing codec stdin")?
                .write_all(&command)
                .await?;
            let mut reply = [0; 8];
            self.codec_out
                .as_mut()
                .context("missing codec stdout")?
                .read_exact(&mut reply)
                .await?;
            ensure!(&reply[..4] == b"JPS1", "invalid JPEG publication header");
            let size = u32::from_le_bytes(reply[4..].try_into().unwrap()) as usize;
            ensure!(
                (4..=crate::protocol::MAX_BODY).contains(&size),
                "invalid JPEG length"
            );
            // Worker cannot reuse output until the next explicit request. Publish an owned copy.
            let jpeg = self.output.as_ref().context("output released")?.bytes()[..size].to_vec();
            ensure!(
                jpeg.starts_with(&[255, 216]) && jpeg.ends_with(&[255, 217]),
                "incomplete JPEG"
            );
            let encoded = began.elapsed();
            self.encode_seconds += encoded.as_secs_f64();
            if let Some(sample) = &mut timing {
                sample.encode = encoded;
            }
            #[cfg(feature = "frame-diagnostics")]
            if let (Some((snapshot, started)), Some(probe)) = (snapshot, &mut self.diagnostic) {
                probe.after(
                    self.encoded + 1,
                    snapshot,
                    self.input.as_ref().unwrap().bytes(),
                    &jpeg,
                    started,
                )?;
            }
            self.encoded += 1;
            self.socket_timing = timing;
            Ok(jpeg)
        };
        let result = tokio::select! {
            value = timeout(duration, operation) => value.unwrap_or_else(|_| Err(anyhow::anyhow!("读取／编码超时"))),
            _ = stop.wait() => Err(Stop::error()),
        };
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    #[cfg(feature = "frame-diagnostics")]
    pub fn flush_diagnostic(&mut self) -> Result<()> {
        ensure!(
            self.codec.is_none()
                && self.helper.is_none()
                && self.input.is_none()
                && self.output.is_none(),
            "diagnostic flush before source cleanup"
        );
        if let Some(mut probe) = self.diagnostic.take() {
            probe.flush()?;
        }
        Ok(())
    }

    pub async fn close(&mut self) -> Result<()> {
        self.failed = true;
        self.raw.take();
        self.request.take();
        self.staging = Vec::new();
        self.codec_in.take();
        self.codec_out.take();
        // Reap both before unmapping, including when the first wait returns an error.
        let a = if let Some(child) = self.codec.as_mut() {
            reap(child).await
        } else {
            Ok(())
        };
        let b = if let Some(child) = self.helper.as_mut() {
            reap(child).await
        } else {
            Ok(())
        };
        if a.is_ok() && b.is_ok() {
            self.codec.take();
            self.helper.take();
            self.input.take();
            self.output.take();
        }
        a.and(b)
    }
}
fn validate_size(w: u32, h: u32) -> Result<()> {
    ensure!(
        (16..=4096).contains(&w)
            && (16..=4096).contains(&h)
            && w as usize * h as usize <= MAX_PIXELS,
        "输出尺寸超过安全限制"
    );
    Ok(())
}
#[cfg(feature = "frame-diagnostics")]
fn cursor_stage_event(record: &[u8; 92], frame: u32) -> Result<serde_json::Value> {
    let word = |i: usize| u32::from_le_bytes(record[4 + i * 4..8 + i * 4].try_into().unwrap());
    ensure!(
        &record[..4] == b"CST2" && word(0) == frame,
        "invalid cursor-stage sidecar"
    );
    let pre = [word(1) as i32, word(2) as i32];
    let post = [word(3) as i32, word(4) as i32];
    let bbox = [word(6), word(7), word(8), word(9)];
    let overlap = word(10);
    ensure!(
        pre.iter()
            .chain(post.iter())
            .all(|n| (-255..=255).contains(n))
            && (bbox == [0; 4]
                || (bbox[0] < bbox[2] && bbox[2] <= 2360 && bbox[1] < bbox[3] && bbox[3] <= 1640))
            && (bbox != [0; 4] || overlap == 0)
            && (word(5) != 0 || bbox == [0; 4])
            && overlap <= 3
            && word(11) != 0,
        "invalid cursor-stage values"
    );
    let segment = |index| -> Result<serde_json::Value> {
        let [x, y, width, contrast] = [
            word(index),
            word(index + 1),
            word(index + 2),
            word(index + 3),
        ];
        if [x, y, width, contrast] == [0; 4] {
            return Ok(serde_json::Value::Null);
        }
        ensure!(
            (449..960).contains(&x)
                && (960..1216).contains(&y)
                && (5..=45).contains(&width)
                && x + width < 960
                && (40..=255).contains(&contrast),
            "invalid cursor-stage ROI segment"
        );
        let overlaps_union = bbox != [0; 4]
            && x < bbox[2]
            && x + width > bbox[0]
            && y - 1 < bbox[3]
            && y + 2 > bbox[1];
        Ok(
            serde_json::json!({"x":x,"y":y,"width":width,"min_contrast_after":contrast,
            "triplet_intersects_cursor_union":overlaps_union}),
        )
    };
    let present = segment(14)?;
    let new = segment(18)?;
    ensure!(
        word(12) <= 512 * 256 / 6
            && word(13) <= word(12)
            && word(12) - word(13) >= u32::from(!present.is_null()) + u32::from(!new.is_null()),
        "invalid cursor-stage ROI counts"
    );
    Ok(serde_json::json!({
        "event": "diagnostic_cursor_stage", "probe_version": 2,
        "roi": {"xyxy":[448,960,960,1216],"after_count":word(12),"mixed_count":word(13),
            "present_before":present,"new_after":new}, "frame": frame,
        "primary_fb_id": word(11), "cursor_planes_composited": word(5),
        "cursor_clipped_union_xyxy": if bbox == [0; 4] { None } else { Some(bbox) },
        "cursor_triplet_overlap_mask": overlap,
        "min_contrast_before": pre, "min_contrast_after": post,
        "known_coordinate_hit": pre.iter().chain(post.iter()).any(|n| *n >= 40),
    }))
}

fn validate_raw_header(h: &[u8; 16], width: u32, height: u32) -> Result<()> {
    let word = |n| u32::from_le_bytes(h[n..n + 4].try_into().unwrap());
    if &h[..4] != b"VKM1" || word(4) != width || word(8) != height || word(12) != width * height * 4
    {
        bail!("原始帧格式／尺寸变化，停止会话");
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    #[cfg(feature = "frame-diagnostics")]
    #[test]
    fn cursor_stage_fixture_and_wire_validation() {
        let fixture = Fixture::new();
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let exe = fixture.0.join("cursor-stage-fixture");
        assert!(
            std::process::Command::new("/usr/bin/cc")
                .args(["-std=c11", "-Wall", "-Wextra", "-Werror", "-I"])
                .arg(manifest.parent().unwrap())
                .arg(manifest.join("tests/cursor_stage_fixture.c"))
                .arg("-o")
                .arg(&exe)
                .status()
                .unwrap()
                .success()
        );
        assert!(std::process::Command::new(exe).status().unwrap().success());
        let mut record = [0; 92];
        record[..4].copy_from_slice(b"CST2");
        for (i, word) in [
            270i32, 170, -30, 170, 225, 1, 880, 1031, 896, 1032, 2, 42, 2, 0, 528, 1013, 16, 133,
            880, 1031, 16, 225,
        ]
        .into_iter()
        .enumerate()
        {
            record[4 + i * 4..8 + i * 4].copy_from_slice(&word.to_le_bytes());
        }
        let event = cursor_stage_event(&record, 270).unwrap();
        assert_eq!(event["min_contrast_before"], serde_json::json!([170, -30]));
        assert_eq!(event["min_contrast_after"], serde_json::json!([170, 225]));
        assert_eq!(event["cursor_triplet_overlap_mask"], 2);
        assert_eq!(event["known_coordinate_hit"], true);
        assert_eq!(
            event["roi"]["present_before"]["triplet_intersects_cursor_union"],
            false
        );
        assert_eq!(
            event["roi"]["new_after"]["triplet_intersects_cursor_union"],
            true
        );
        let mut invalid_segment = record;
        invalid_segment[68..72].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(cursor_stage_event(&invalid_segment, 270).is_err());
        assert!(cursor_stage_event(&record, 269).is_err());
        record[44] = 4;
        assert!(cursor_stage_event(&record, 270).is_err());
        record[0] = 0;
        assert!(cursor_stage_event(&record, 270).is_err());
    }
    pub(crate) struct Fixture(pub std::path::PathBuf);
    impl Fixture {
        pub(crate) fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "xdisplay-rust-source-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
            let compiled = std::process::Command::new("/usr/bin/cc")
                .args(["-std=c11", "-Wall", "-Wextra", "-Werror"])
                .arg(manifest.join("tests/raw_fixture.c"))
                .arg("-o")
                .arg(path.join("vkms-read"))
                .status()
                .unwrap();
            assert!(compiled.success());
            assert!(
                std::process::Command::new("/usr/bin/cc")
                    .args(["-std=c11", "-O2", "-Wall", "-Wextra", "-Werror"])
                    .arg(manifest.parent().unwrap().join("turbo_jpeg.c"))
                    .arg("-o")
                    .arg(path.join("turbo-jpeg"))
                    .arg("-lturbojpeg")
                    .status()
                    .unwrap()
                    .success()
            );
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).expect("remove owned synthetic fixture only");
        }
    }
    pub(crate) fn settings(mode: u32, transport: &str) -> Settings {
        Settings {
            device: "64x32".into(),
            connector: mode,
            transport: transport.into(),
            frames: 6,
            ..Settings::default()
        }
    }
    #[tokio::test]
    #[ignore = "explicit offline performance measurement; run in release mode"]
    async fn socket_staging_abba() {
        let fixture = Fixture::new();
        let stop = Stop::default();
        let mut reference = None;
        for reuse in [false, true, true, false, true, false, false, true] {
            let mut s = settings(1, "socket");
            s.device = "2360x1640".into();
            s.frames = 28;
            s.socket_timing = true;
            s.socket_reuse_staging = reuse;
            let mut source = Source::new(&s, 2360, 1640).unwrap();
            source.start(&fixture.0, &stop).await.unwrap();
            let mut jpegs = Vec::new();
            let mut rows = Vec::new();
            let mut address = None;
            for index in 0..s.frames {
                let start = Instant::now();
                let jpeg = source
                    .next(2360, 1640, Duration::from_secs(3), &stop)
                    .await
                    .unwrap();
                let elapsed = start.elapsed();
                if reuse {
                    let ptr = source.staging.as_ptr();
                    assert_eq!(source.staging.len(), 2360 * 1640 * 4);
                    assert_eq!(*address.get_or_insert(ptr), ptr);
                }
                if index >= 4 {
                    let t = source.socket_timing.take().unwrap();
                    rows.push([elapsed, t.allocate, t.receive, t.copy, t.encode]);
                }
                jpegs.push(jpeg);
            }
            source.close().await.unwrap();
            assert!(source.helper.is_none() && source.codec.is_none());
            assert!(source.input.is_none() && source.output.is_none());
            if let Some(reference) = &reference {
                assert_eq!(
                    &jpegs, reference,
                    "synthetic sequence must remain byte identical"
                );
            } else {
                reference = Some(jpegs);
            }
            let mut cols = Vec::new();
            for column in 0..5 {
                let mut v: Vec<_> = rows.iter().map(|r| r[column]).collect();
                v.sort_unstable();
                cols.push([12, 23].map(|rank| v[rank - 1].as_secs_f64() * 1000.));
            }
            println!("reuse={reuse} [p50_ms,p95_ms] total/alloc/recv/copy/encode: {cols:.3?}");
        }
    }

    #[tokio::test]
    async fn native_pipeline_same_jpeg_and_owned_output() {
        let fixture = Fixture::new();
        let stop = Stop::default();
        for quality in [75, 85, 90] {
            let mut sequences = Vec::new();
            for (transport, measured) in [
                ("socket", false),
                ("socket", true),
                ("socket-direct", false),
            ] {
                let mut settings = settings(1, transport);
                settings.quality = quality;
                settings.socket_timing = measured;
                let mut source = Source::new(&settings, 64, 32).unwrap();
                source.start(&fixture.0, &stop).await.unwrap();
                assert!(
                    source.codec.is_none(),
                    "codec must not time out while authorization is pending"
                );
                let mut frames = Vec::new();
                for index in 0..6 {
                    frames.push(
                        source
                            .next(64, 32, Duration::from_secs(2), &stop)
                            .await
                            .unwrap(),
                    );
                    assert_eq!(source.socket_timing.is_some(), measured && index > 0);
                }
                assert!(frames.windows(2).all(|p| p[0] != p[1]));
                source.close().await.unwrap();
                assert!(source.input.is_none() && source.output.is_none());
                assert!(source.helper.is_none() && source.codec.is_none());
                assert!(
                    frames
                        .iter()
                        .all(|j| j.starts_with(&[255, 216]) && j.ends_with(&[255, 217]))
                );
                sequences.push(frames);
            }
            assert_eq!(sequences[0], sequences[1]);
            assert_eq!(sequences[0], sequences[2]);
        }
    }
    #[tokio::test]
    async fn partial_eof_and_resize_poison_source_and_release() {
        let fixture = Fixture::new();
        for (mode, reuse) in [(2, false), (3, false), (2, true), (3, true)] {
            let mut s = settings(mode, if reuse { "socket" } else { "socket-direct" });
            s.socket_reuse_staging = reuse;
            let mut source = Source::new(&s, 64, 32).unwrap();
            let stop = Stop::default();
            source.start(&fixture.0, &stop).await.unwrap();
            for _ in 0..2 {
                source
                    .next(64, 32, Duration::from_secs(2), &stop)
                    .await
                    .unwrap();
            }
            assert!(
                source
                    .next(64, 32, Duration::from_secs(2), &stop)
                    .await
                    .is_err()
            );
            assert_eq!(source.encoded, 2);
            assert!(
                source
                    .next(64, 32, Duration::from_secs(2), &stop)
                    .await
                    .is_err()
            );
            source.close().await.unwrap();
            assert!(source.input.is_none() && source.output.is_none());
            source.close().await.unwrap();
        }
    }
    #[tokio::test]
    async fn cancellation_during_raw_read_reaps_before_unmap() {
        let fixture = Fixture::new();
        let stop = Stop::default();
        let request = stop.clone();
        let mut source = Source::new(&settings(4, "socket-direct"), 64, 32).unwrap();
        source.start(&fixture.0, &stop).await.unwrap();
        let cancel = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(25)).await;
            request.request();
        });
        assert!(
            source
                .next(64, 32, Duration::from_secs(2), &stop)
                .await
                .is_err()
        );
        assert_eq!(source.encoded, 0);
        source.close().await.unwrap();
        cancel.await.unwrap();
        assert!(source.input.is_none());
    }
    #[test]
    fn checked_geometry_and_mapping() {
        assert!(validate_size(0, 16).is_err());
        assert!(validate_size(4096, 4096).is_err());
        let mut map = Mapping::new(4096, true).unwrap();
        map.bytes_mut()[0] = 42;
        assert_eq!(map.bytes()[0], 42);
        assert_eq!(unsafe { libc::ftruncate(map.fd.as_raw_fd(), 1) }, -1);
    }
    #[test]
    fn frame_header_rejects_partial_and_resize() {
        let mut h = [0; 16];
        h[..4].copy_from_slice(b"VKM1");
        for (i, v) in [(4, 16u32), (8, 16), (12, 1024)] {
            h[i..i + 4].copy_from_slice(&v.to_le_bytes());
        }
        assert!(validate_raw_header(&h, 16, 16).is_ok());
        assert!(validate_raw_header(&h, 32, 16).is_err());
        h[0] = 0;
        assert!(validate_raw_header(&h, 16, 16).is_err());
    }
}
