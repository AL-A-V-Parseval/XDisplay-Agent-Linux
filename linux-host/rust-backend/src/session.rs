//! Bounded JPEG session. Benign input never cancels a partially received frame.
// Production has no fixture endpoint or arbitrary helper command override.
use crate::{
    Settings, emit, inventory, mux,
    process::Stop,
    protocol::{self, Protocol},
    source::Source,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::json;

#[derive(Debug)]
pub struct SourceFailure(anyhow::Error);
impl std::fmt::Display for SourceFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "VKMS／编码器：{}", self.0)
    }
}
impl std::error::Error for SourceFailure {}
fn source_failure(error: anyhow::Error) -> anyhow::Error {
    if error.is::<crate::process::Cancelled>() {
        error
    } else {
        SourceFailure(error).into()
    }
}
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    io::AsyncWriteExt,
    net::UnixStream,
    sync::{mpsc, watch},
    time::{Instant as TokioInstant, sleep_until, timeout},
};
#[derive(Clone, Default)]
struct State {
    order: bool,
    ready: bool,
    error: Option<String>,
}
fn check(state: &watch::Receiver<State>, stop: &Stop) -> Result<()> {
    stop.check()?;
    if let Some(message) = &state.borrow().error {
        bail!("{message}");
    }
    Ok(())
}
async fn failure(state: &mut watch::Receiver<State>) {
    loop {
        if state.borrow().error.is_some() {
            return;
        }
        if state.changed().await.is_err() {
            return;
        }
    }
}

pub async fn stream(root: &Path, settings: &Settings, stop: &Stop) -> Result<()> {
    stream_with_tunnel(root, settings, stop, None).await
}

pub async fn stream_with_tunnel(
    root: &Path,
    settings: &Settings,
    stop: &Stop,
    tunnel: Option<UnixStream>,
) -> Result<()> {
    emit(json!({"event":"phase","phase":"validating"}));
    let info = inventory(root, stop).await?;
    emit(info.clone());
    let selected = info["outputs"]
        .as_array()
        .context("invalid outputs")?
        .iter()
        .find(|v| v["device"] == settings.device && v["connector"] == settings.connector)
        .context("所选VKMS输出已失效")?;
    ensure!(
        info["devices"]
            .as_array()
            .context("invalid devices")?
            .iter()
            .any(|v| v["device_id"] == settings.device_id),
        "所选USB设备已离线"
    );
    let width = selected["width"].as_u64().context("missing width")? as u32;
    let height = selected["height"].as_u64().context("missing height")? as u32;
    let mut source = Source::new(settings, width, height)?;
    #[cfg(feature = "frame-diagnostics")]
    if settings.diagnostic_dir.is_some() {
        emit(
            json!({"event":"diagnostic_config", "device":settings.device,
            "connector":settings.connector, "device_id":settings.device_id,
            "quality":settings.quality,"fps":settings.fps,"transport":settings.transport,
            "elevate":settings.elevate,"frames":settings.frames,"seconds":settings.seconds,
            "width":width,"height":height}),
        );
    }
    let result = async {
        emit(json!({"event":"phase","phase":"preparing"}));
        let first = if settings.continuous {
            Vec::new() // No frame capture or authorization until the App handshake succeeds.
        } else {
            source.start(root, stop).await?;
            source
                .next(width, height, Duration::from_secs(65), stop)
                .await?
        };
        emit(json!({"event":"phase","phase":"waiting"}));
        let tunnel = match tunnel {
            Some(tunnel) => tunnel,
            None => mux::connect(settings.device_id, stop).await?,
        };
        drive_inner(tunnel, &mut source, first, settings, stop, root).await
    }
    .await;
    emit(json!({"event":"phase","phase":"cleaning"}));
    let cleanup = source.close().await;
    emit(json!({"event":"cleanup","complete":cleanup.is_ok()}));
    cleanup.map_err(|error| SourceFailure(error))?;
    #[cfg(feature = "frame-diagnostics")]
    if settings.diagnostic_dir.is_some() {
        source.flush_diagnostic()?;
        emit(json!({"event":"diagnostic_saved"}));
    }
    let count = result?;
    emit(json!({"event":"finished","frames":count,"display_verified":false}));
    Ok(())
}

const TIMING_FIELDS: [&str; 9] = [
    "request_to_header_ms",
    "staging_allocate_ms",
    "payload_read_ms",
    "staging_copy_release_ms",
    "encode_publish_ms",
    "usb_tunnel_write_ms",
    "pacing_wait_ms",
    "send_interval_ms",
    "jpeg_bytes",
];

fn timing_summary(samples: &[[f64; 9]]) -> serde_json::Value {
    let mut stats = serde_json::Map::new();
    if !samples.is_empty() {
        for (index, key) in TIMING_FIELDS.into_iter().enumerate() {
            let mut values: Vec<_> = samples.iter().map(|row| row[index]).collect();
            values.sort_unstable_by(f64::total_cmp);
            let rank = |percent: usize| values[(percent * values.len()).div_ceil(100) - 1];
            stats.insert(key.into(), json!({"p50":rank(50),"p95":rank(95)}));
        }
    }
    json!({"event":"socket_timing","samples":samples.len(),"stats":stats,
        "scope":"host_only_excludes_prepared_frame_and_incomplete_writes"})
}

fn pace_after_frame(
    deadline: TokioInstant,
    period: Duration,
    now: TokioInstant,
    skip_expired: bool,
) -> (TokioInstant, bool) {
    let next = (deadline + period).max(now);
    (next, !skip_expired || next > now)
}

// The caller owns the stream until the whole packet completes. On cancellation
// it closes the tunnel rather than starting another packet after a partial write.
async fn write_video<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    header: &[u8; 4],
    jpeg: &[u8],
    vectored: bool,
) -> std::io::Result<()> {
    if !vectored {
        writer.write_all(header).await?;
        return writer.write_all(jpeg).await;
    }
    let mut offset = 0;
    while offset < header.len() {
        let parts = [
            std::io::IoSlice::new(&header[offset..]),
            std::io::IoSlice::new(jpeg),
        ];
        let written = writer.write_vectored(&parts).await?;
        if written == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        offset += written;
    }
    writer.write_all(&jpeg[offset - header.len()..]).await
}

#[cfg(test)]
async fn drive(
    tunnel: UnixStream,
    source: &mut Source,
    first: Vec<u8>,
    settings: &Settings,
    stop: &Stop,
) -> Result<u32> {
    drive_inner(tunnel, source, first, settings, stop, Path::new("")).await
}

async fn drive_inner(
    tunnel: UnixStream,
    source: &mut Source,
    first: Vec<u8>,
    settings: &Settings,
    stop: &Stop,
    root: &Path,
) -> Result<u32> {
    let (mut reader, mut writer) = tunnel.into_split();
    let protocol = Arc::new(Mutex::new(Protocol::default()));
    let shared = protocol.clone();
    let (tx, mut responses) = mpsc::channel::<Vec<u8>>(8);
    let (state_tx, mut state) = watch::channel(State::default());
    let receiver_stop = stop.clone();
    let receiver = tokio::spawn(async move {
        let mut status = State::default();
        let result: Result<()> = async {
            loop {
                let (tag, body) = tokio::select! {
                    _ = receiver_stop.wait() => return Err(Stop::error()),
                    packet = protocol::read_packet(&mut reader,None) => packet?,
                };
                let replies = {
                    let mut p = shared
                        .lock()
                        .map_err(|_| anyhow::anyhow!("protocol state poisoned"))?;
                    let replies = p.handle(tag, &body)?;
                    status.order |= tag == 4;
                    status.ready = p.streaming;
                    replies
                };
                state_tx.send_replace(status.clone());
                for reply in replies {
                    tx.send(reply).await.context("session writer closed")?;
                }
            }
        }
        .await;
        if let Err(error) = result {
            status.error = Some(error.to_string());
            state_tx.send_replace(status);
        }
    });
    let mut drained = 0;
    let mut sending_started = None;
    let mut samples = Vec::with_capacity(if settings.socket_timing {
        settings.frames as usize
    } else {
        0
    });
    let mut last_write = None;
    let mut pacing = Duration::ZERO;
    let mut expired_waits_skipped = 0u32;
    let ms = |duration: Duration| duration.as_secs_f64() * 1000.;
    let operation = async {
        let handshake = async {
            writer.write_all(&protocol::packet(4, &[])?).await?;
            // Mirror the established host: allow the byte-order marker up to 1s.
            let order = async {
                while !state.borrow().order {
                    check(&state, stop)?;
                    state.changed().await?;
                }
                Ok::<_, anyhow::Error>(())
            };
            if let Ok(result) = timeout(Duration::from_secs(1), order).await {
                result?;
            }
            check(&state, stop)?;
            let hello = protocol.lock().unwrap().hello();
            writer.write_all(&hello).await?;
            while !state.borrow().ready {
                check(&state, stop)?;
                tokio::select! {
                    _ = stop.wait() => return Err(Stop::error()),
                    changed = state.changed() => { changed?; },
                    reply = responses.recv() => writer.write_all(&reply.context("receiver stopped")?).await?,
                }
            }
            check(&state, stop)?;
            Ok::<_, anyhow::Error>(())
        };
        tokio::select! {
            _ = stop.wait() => return Err(Stop::error()),
            result = timeout(Duration::from_secs(10), handshake) => result.context("等待 iPad 握手超时")??,
        }
        let (width, height) = protocol.lock().unwrap().size.context("缺少协商尺寸")?;
        ensure!(
            source.size() == (width, height),
            "协商画布与虚拟输出不同；不会隐式缩放"
        );
        let first = if settings.continuous {
            emit(json!({"event":"phase","phase":"preparing"}));
            tokio::select! {
                value = async {
                    source.start(root, stop).await.map_err(source_failure)?;
                    source.next(width, height, Duration::from_secs(65), stop).await.map_err(source_failure)
                } => value?,
                _ = failure(&mut state) => { check(&state, stop)?; bail!("receiver stopped"); },
                _ = stop.wait() => return Err(Stop::error()),
            }
        } else {
            first
        };
        emit(
            json!({"event":"phase","phase":"sending","width":width,"height":height,"input":"off","codec":"jpeg"}),
        );
        let started = Instant::now();
        sending_started = Some(started);
        let mut last_report = started;
        let period = Duration::from_secs_f64(1. / settings.fps as f64);
        let mut deadline = TokioInstant::now();
        let mut cached = Some(first);
        let cap = if settings.continuous {
            u32::MAX
        } else {
            settings.frames
        };
        for index in 0..cap {
            check(&state, stop)?;
            for _ in 0..8 {
                let Ok(reply) = responses.try_recv() else {
                    break;
                };
                tokio::select! {
                    _ = stop.wait() => return Err(Stop::error()),
                    result = timeout(Duration::from_secs(3), writer.write_all(&reply)) => result.context("USB 控制写入超时")??,
                }
            }
            let frame = async {
                let jpeg = if let Some(jpeg) = cached.take() {
                    ensure!(
                        protocol.lock().unwrap().size == Some((width, height)),
                        "画布变化"
                    );
                    jpeg
                } else {
                    source
                        .next(width, height, Duration::from_secs(12), stop)
                        .await
                        .map_err(source_failure)?
                };
                // Validate negotiated size even when the prepared JPEG is reused.
                // source.next refuses resizing; the prepared dimensions were selected from actual VKMS.
                let header =
                    protocol::header(if protocol.lock().unwrap().big { 6 } else { 2 }, jpeg.len())?;
                let write_started = settings.socket_timing.then(Instant::now);
                timeout(
                    Duration::from_secs(8),
                    write_video(&mut writer, &header, &jpeg, settings.socket_writev),
                )
                .await
                .context("USB 视频写入超时")??;
                if let Some(write_started) = write_started {
                    let completed = Instant::now();
                    let previous = last_write.replace(completed);
                    if let (Some(previous), Some(sample)) = (previous, source.socket_timing.take())
                    {
                        samples.push([
                            ms(sample.header),
                            ms(sample.allocate),
                            ms(sample.receive),
                            ms(sample.copy),
                            ms(sample.encode),
                            ms(completed - write_started),
                            ms(pacing),
                            ms(completed - previous),
                            jpeg.len() as f64,
                        ]);
                    }
                }
                Ok::<_, anyhow::Error>(())
            };
            tokio::select! {
                result = frame => result?,
                _ = failure(&mut state) => { check(&state,stop)?; bail!("receiver stopped"); },
                _ = stop.wait() => return Err(Stop::error()),
            }
            let frames = index + 1;
            drained = frames;
            #[cfg(feature = "frame-diagnostics")]
            if settings.diagnostic_dir.is_some() && crate::diagnostic::Diagnostic::selected(frames)
            {
                let host_unix_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|d| d.as_millis() as u64);
                emit(json!({"event":"diagnostic_sent", "frame":frames,
                    "host_elapsed_ms":started.elapsed().as_secs_f64()*1000., "host_unix_ms":host_unix_ms}));
            }
            if last_report.elapsed() >= Duration::from_secs(1) || frames == settings.frames {
                emit(
                    json!({"event":"metrics","frames":frames,"send_fps":frames as f64/started.elapsed().as_secs_f64(),
                    "encode_ms":source.encode_seconds*1000./source.encoded.max(1) as f64,
                    "display_latency_verified":false}),
                );
                last_report = Instant::now();
            }
            let (next, should_wait) = pace_after_frame(
                deadline,
                period,
                TokioInstant::now(),
                settings.socket_skip_expired_wait,
            );
            deadline = next;
            if frames < cap && should_wait {
                let wait_started = settings.socket_timing.then(Instant::now);
                tokio::select! {
                    _ = sleep_until(deadline) => {},
                    _ = failure(&mut state) => { check(&state,stop)?; bail!("receiver stopped"); },
                    _ = stop.wait() => return Err(Stop::error()),
                }
                if let Some(wait_started) = wait_started {
                    pacing = wait_started.elapsed();
                }
            } else if frames < settings.frames {
                expired_waits_skipped += 1;
                pacing = Duration::ZERO;
            }
        }
        Ok(drained)
    };
    let outcome = if settings.continuous {
        Ok(operation.await)
    } else {
        timeout(Duration::from_secs(settings.seconds as u64), operation).await
    };
    let result = match outcome {
        Ok(result) => result,
        Err(_) if drained > 0 => {
            emit(
                json!({"event":"metrics","frames":drained,"send_fps":drained as f64 / sending_started.unwrap().elapsed().as_secs_f64(),
                "encode_ms":source.encode_seconds*1000./source.encoded.max(1) as f64,"display_verified":false}),
            );
            Ok(drained)
        }
        Err(_) => Err(anyhow::anyhow!(
            "会话超时，尚未发送视频；请确认iPad App已打开"
        )),
    };
    receiver.abort();
    let _ = receiver.await;
    let _ = timeout(Duration::from_secs(1), writer.shutdown()).await;
    if settings.socket_timing {
        let mut summary = timing_summary(&samples);
        summary["frames_written"] = json!(drained);
        summary["width"] = json!(source.size().0);
        summary["height"] = json!(source.size().1);
        summary["quality"] = json!(settings.quality);
        summary["target_fps"] = json!(settings.fps);
        summary["skip_expired_wait"] = json!(settings.socket_skip_expired_wait);
        summary["writev"] = json!(settings.socket_writev);
        summary["reuse_staging"] = json!(settings.socket_reuse_staging);
        summary["expired_waits_skipped"] = json!(expired_waits_skipped);
        summary["complete"] = json!(drained == settings.frames && result.is_ok());
        emit(summary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn video_writev_preserves_packet_under_short_writes_and_backpressure() {
        let header = [6, 4, 0, 0];
        let jpeg: Vec<u8> = (0..(256 * 1024)).map(|n| (n % 251) as u8).collect();
        let expected = [&header[..], &jpeg[..]].concat();
        timeout(Duration::from_secs(5), async {
            // The duplex fallback can accept only two bytes, splitting the header.
            let (mut tiny, mut reader) = tokio::io::duplex(2);
            let mut bytes = [0; 9];
            let (sent, received) = tokio::join!(
                write_video(&mut tiny, &header, &jpeg[..5], true),
                reader.read_exact(&mut bytes)
            );
            sent.unwrap();
            received.unwrap();
            assert_eq!(bytes, expected[..9]);

            for writev in [false, true] {
                let (host, mut peer) = UnixStream::pair().unwrap();
                let (_reader, mut host) = host.into_split();
                assert!(tokio::io::AsyncWrite::is_write_vectored(&host));
                let mut bytes = vec![0; expected.len()];
                let (sent, received) = tokio::join!(
                    write_video(&mut host, &header, &jpeg, writev),
                    peer.read_exact(&mut bytes)
                );
                sent.unwrap();
                received.unwrap();
                assert_eq!(bytes, expected);
            }
        })
        .await
        .unwrap();
    }

    #[test]
    fn pacing_keeps_future_deadlines_and_rebases_misses_without_catch_up() {
        let start = TokioInstant::now();
        let period = Duration::from_millis(16);
        let next = start + period;
        assert_eq!(pace_after_frame(start, period, start, true), (next, true));
        assert_eq!(pace_after_frame(start, period, next, true), (next, false));
        let late = start + Duration::from_millis(40);
        assert_eq!(pace_after_frame(start, period, late, false), (late, true));
        assert_eq!(pace_after_frame(start, period, late, true), (late, false));
        assert_eq!(
            pace_after_frame(late, period, late, true),
            (late + period, true)
        );
    }

    #[test]
    fn socket_timing_nearest_rank_and_empty_session() {
        let samples: Vec<_> = (1..=20).rev().map(|n| [n as f64; 9]).collect();
        let result = timing_summary(&samples);
        assert_eq!(result["samples"], 20);
        assert_eq!(result["stats"]["jpeg_bytes"], json!({"p50":10.,"p95":19.}));
        assert_eq!(timing_summary(&[])["stats"], json!({}));
    }
    fn command(big: bool, values: [u32; 3]) -> Vec<u8> {
        let body = values
            .into_iter()
            .flat_map(|v| {
                if big {
                    v.to_be_bytes()
                } else {
                    v.to_le_bytes()
                }
            })
            .collect::<Vec<_>>();
        protocol::packet(if big { 5 } else { 1 }, &body).unwrap()
    }
    async fn mobile(
        mut socket: UnixStream,
        big: bool,
        width: u32,
        frames: u32,
    ) -> Result<Vec<Vec<u8>>> {
        let probe = protocol::read_packet(&mut socket, None).await?;
        ensure!(probe == (4, vec![]), "probe");
        if big {
            socket.write_all(&protocol::packet(4, &[])?).await?;
        }
        let hello = protocol::read_packet(&mut socket, None).await?;
        ensure!(hello.0 == if big { 5 } else { 1 }, "hello order");
        socket.write_all(&command(big, [2, width, 32])).await?;
        let ack = protocol::read_packet(&mut socket, None).await?;
        ensure!(ack.0 == if big { 5 } else { 1 }, "ack order");
        socket.write_all(&command(big, [255, 0, 0])).await?;
        let mut results = Vec::new();
        for _ in 0..frames {
            // Benign input in flight must not restart a partially received source frame.
            let mut input = command(big, [4, 0, 0]);
            input[0] = if big { 7 } else { 3 };
            socket.write_all(&input).await?;
            let (tag, body) = protocol::read_packet(&mut socket, None).await?;
            ensure!(tag == if big { 6 } else { 2 }, "JPEG tag");
            ensure!(
                body.starts_with(&[255, 216]) && body.ends_with(&[255, 217]),
                "complete JPEG"
            );
            results.push(body);
        }
        let mut end = [0];
        let _ = timeout(Duration::from_secs(2), socket.read(&mut end)).await;
        Ok(results)
    }
    #[tokio::test]
    async fn complete_session_be_and_le_with_native_jpeg_and_benign_controls() {
        let fixture = crate::source::tests::Fixture::new();
        let mut reference = None;
        for (big, measured, writev) in [
            (false, false, false),
            (true, false, false),
            (true, true, false),
            (true, true, true),
            (false, true, true),
        ] {
            let mut settings = crate::source::tests::settings(
                1,
                if measured { "socket" } else { "socket-direct" },
            );
            settings.socket_timing = measured;
            settings.socket_writev = writev;
            settings.seconds = 4;
            let stop = Stop::default();
            let mut source = Source::new(&settings, 64, 32).unwrap();
            source.start(&fixture.0, &stop).await.unwrap();
            let first = source
                .next(64, 32, Duration::from_secs(2), &stop)
                .await
                .unwrap();
            let (host, peer) = UnixStream::pair().unwrap();
            let remote = tokio::spawn(mobile(peer, big, 64, settings.frames));
            let count = drive(host, &mut source, first, &settings, &stop)
                .await
                .unwrap();
            assert_eq!(count, 6);
            let images = remote.await.unwrap().unwrap();
            assert_eq!(images.len(), 6);
            assert!(images.windows(2).all(|p| p[0] != p[1]));
            if let Some(reference) = &reference {
                assert_eq!(&images, reference);
            } else {
                reference = Some(images);
            }
            assert_eq!(source.encoded, 6);
            source.close().await.unwrap();
        }
    }
    #[tokio::test]
    async fn continuous_passes_300_on_one_tunnel_then_reaps_on_stop() {
        let fixture = crate::source::tests::Fixture::new();
        let mut settings = crate::source::tests::settings(1, "socket");
        settings.continuous = true;
        settings.socket_reuse_staging = true;
        settings.frames = 300;
        settings.seconds = 1; // The manual test limit must not apply.
        let stop = Stop::default();
        let mut source = Source::new(&settings, 64, 32).unwrap();
        let (host, peer) = UnixStream::pair().unwrap();
        let peer_stop = stop.clone();
        let remote = tokio::spawn(async move {
            let images = mobile(peer, true, 64, 306).await.unwrap();
            peer_stop.request();
            images
        });
        let result = drive_inner(host, &mut source, Vec::new(), &settings, &stop, &fixture.0).await;
        source.close().await.unwrap();
        assert_eq!(remote.await.unwrap().len(), 306);
        assert!(source.encoded >= 306);
        assert!(result.is_err()); // Stop or peer EOF, never the manual frame or time limit.
    }

    #[tokio::test]
    async fn prepared_frame_rejects_resolution_mismatch_before_any_video() {
        let fixture = crate::source::tests::Fixture::new();
        let settings = crate::source::tests::settings(1, "socket-direct");
        let stop = Stop::default();
        let mut source = Source::new(&settings, 64, 32).unwrap();
        source.start(&fixture.0, &stop).await.unwrap();
        let first = source
            .next(64, 32, Duration::from_secs(2), &stop)
            .await
            .unwrap();
        let (host, peer) = UnixStream::pair().unwrap();
        let remote = tokio::spawn(mobile(peer, true, 80, 1));
        assert!(
            drive(host, &mut source, first, &settings, &stop)
                .await
                .is_err()
        );
        assert!(remote.await.unwrap().is_err());
        assert_eq!(source.encoded, 1);
        source.close().await.unwrap();
    }
    #[tokio::test]
    async fn session_deadline_after_video_is_success_but_empty_session_fails() {
        let fixture = crate::source::tests::Fixture::new();
        for send_video in [false, true] {
            let mut settings = crate::source::tests::settings(1, "socket-direct");
            settings.frames = 300;
            settings.seconds = 1;
            let stop = Stop::default();
            let mut source = Source::new(&settings, 64, 32).unwrap();
            source.start(&fixture.0, &stop).await.unwrap();
            let first = source
                .next(64, 32, Duration::from_secs(2), &stop)
                .await
                .unwrap();
            let (host, peer) = UnixStream::pair().unwrap();
            let remote = tokio::spawn(async move {
                if send_video {
                    let _ = mobile(peer, true, 64, 300).await;
                } else {
                    tokio::time::sleep(Duration::from_millis(1300)).await;
                    drop(peer);
                }
            });
            let result = drive(host, &mut source, first, &settings, &stop).await;
            if send_video {
                let count = result.unwrap();
                assert!(count > 0 && count < 300);
            } else {
                assert!(result.is_err());
            }
            source.close().await.unwrap();
            remote.await.unwrap();
        }
    }
    #[tokio::test]
    async fn stop_during_handshake_is_typed_and_cleanup_completes() {
        let fixture = crate::source::tests::Fixture::new();
        let settings = crate::source::tests::settings(1, "socket-direct");
        let stop = Stop::default();
        let trigger = stop.clone();
        let mut source = Source::new(&settings, 64, 32).unwrap();
        source.start(&fixture.0, &stop).await.unwrap();
        let first = source
            .next(64, 32, Duration::from_secs(2), &stop)
            .await
            .unwrap();
        let (host, _peer) = UnixStream::pair().unwrap();
        let cancel = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(25)).await;
            trigger.request();
        });
        let error = drive(host, &mut source, first, &settings, &stop)
            .await
            .unwrap_err();
        assert!(error.downcast_ref::<crate::process::Cancelled>().is_some());
        source.close().await.unwrap();
        cancel.await.unwrap();
    }
    #[tokio::test]
    async fn peer_disconnect_and_frame_failure_finish_without_hanging() {
        let fixture = crate::source::tests::Fixture::new();
        for mode in [1, 3] {
            let settings = crate::source::tests::settings(mode, "socket-direct");
            let stop = Stop::default();
            let mut source = Source::new(&settings, 64, 32).unwrap();
            source.start(&fixture.0, &stop).await.unwrap();
            let first = source
                .next(64, 32, Duration::from_secs(2), &stop)
                .await
                .unwrap();
            let (host, peer) = UnixStream::pair().unwrap();
            let remote = tokio::spawn(async move {
                if mode == 1 {
                    drop(peer);
                    Ok(Vec::new())
                } else {
                    mobile(peer, true, 64, 6).await
                }
            });
            assert!(
                timeout(
                    Duration::from_secs(3),
                    drive(host, &mut source, first, &settings, &stop)
                )
                .await
                .unwrap()
                .is_err()
            );
            let _ = remote.await.unwrap();
            source.close().await.unwrap();
        }
    }
}
