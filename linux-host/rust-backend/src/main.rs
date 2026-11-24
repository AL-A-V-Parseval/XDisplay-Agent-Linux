//! GTK JSON-status backend. Native runtime only; no implicit capture or setup.
mod automation;
#[cfg(feature = "frame-diagnostics")]
mod diagnostic;
mod mux;
mod output;
mod process;
mod protocol;
mod session;
mod source;
use anyhow::{Context, Result, bail, ensure};
use process::Stop;
use serde_json::{Value, json};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

static STATUS_CLOSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn emit(value: Value) {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    if serde_json::to_writer(&mut lock, &value).is_err()
        || lock.write_all(b"\n").is_err()
        || lock.flush().is_err()
    {
        STATUS_CLOSED.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}
#[derive(Clone, Debug)]
pub struct Settings {
    pub device: String,
    pub connector: u32,
    pub device_id: u32,
    pub quality: u32,
    pub fps: u32,
    pub transport: String,
    pub elevate: bool,
    pub socket_timing: bool,
    pub socket_writev: bool,
    pub socket_reuse_staging: bool,
    pub socket_skip_expired_wait: bool,
    pub frames: u32,
    pub seconds: u32,
    pub continuous: bool,
    #[cfg(feature = "frame-diagnostics")]
    pub diagnostic_dir: Option<PathBuf>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            device: String::new(),
            connector: 0,
            device_id: 0,
            quality: 85,
            fps: 60,
            transport: "socket-direct".into(),
            elevate: false,
            socket_timing: false,
            socket_writev: false,
            socket_reuse_staging: false,
            socket_skip_expired_wait: false,
            frames: 300,
            seconds: 15,
            continuous: false,
            #[cfg(feature = "frame-diagnostics")]
            diagnostic_dir: None,
        }
    }
}
impl Settings {
    fn validate(&self) -> Result<()> {
        ensure!(valid_card(&self.device), "必须选择明确的VKMS DRM节点");
        ensure!(
            self.connector > 0 && self.device_id > 0,
            "无效设备或连接器ID"
        );
        ensure!([75, 85, 90].contains(&self.quality), "仅支持Q75、Q85、Q90");
        ensure!([30, 60].contains(&self.fps), "仅支持30或60fps");
        ensure!(
            ["socket", "socket-direct"].contains(&self.transport.as_str()),
            "无效JPEG传输方式"
        );
        ensure!(
            (1..=300).contains(&self.frames) && (1..=15).contains(&self.seconds),
            "会话最多300帧／15秒"
        );
        ensure!(
            !(self.socket_timing
                || self.socket_skip_expired_wait
                || self.socket_writev
                || self.socket_reuse_staging)
                || self.transport == "socket",
            "Socket 实验仅支持普通 Socket"
        );
        ensure!(
            !(self.socket_writev && self.socket_reuse_staging),
            "暂存复用与 writev 必须分开测试"
        );
        if self.continuous {
            ensure!(
                self.transport == "socket"
                    && !self.socket_timing
                    && !self.socket_writev
                    && self.socket_reuse_staging
                    && self.frames == 300,
                "长期显示只支持复用暂存的普通 Socket，且不能启用逐帧性能采样"
            );
        }
        #[cfg(feature = "frame-diagnostics")]
        if let Some(dir) = &self.diagnostic_dir {
            ensure!(
                dir.is_absolute()
                    && self.quality == 85
                    && self.fps == 60
                    && self.transport == "socket-direct"
                    && self.frames == 300
                    && self.seconds == 15,
                "诊断要求绝对输出路径及 Q85/60fps/Socket Direct/300帧/15秒"
            );
        }
        Ok(())
    }
}
pub fn valid_card(path: &str) -> bool {
    path.strip_prefix("/dev/dri/card")
        .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
}
fn root() -> Result<PathBuf> {
    // Production installs the binary beside the existing native helpers. A cargo
    // development executable resolves the compile-time project path, not cwd/PATH.
    let exe = std::env::current_exe()?;
    let parent = exe.parent().context("missing executable directory")?;
    if parent.join("vkms-probe").is_file() && parent.join("turbo-jpeg").is_file() {
        return Ok(parent.into());
    }
    Ok(Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("missing project root")?
        .into())
}
pub async fn inventory(root: &Path, stop: &Stop) -> Result<Value> {
    stop.check()?;
    let mut warnings = Vec::new();
    let outputs = match output::scan(root, stop).await {
        Ok(rows) => rows.into_iter().filter(|v| v["active"] == true).map(|v| json!({
            "device":v["device"], "connector":v["connector"], "width":v["width"], "height":v["height"]})).collect::<Vec<_>>(),
        Err(_) => { stop.check()?; warnings.push("虚拟输出元数据检查失败；不会提权或采集"); Vec::new() }
    };
    let devices = match mux::devices(stop).await {
        Ok(rows) => rows
            .into_iter()
            .filter(|v| v["connection_type"] == "USB")
            .collect::<Vec<_>>(),
        Err(_) => {
            stop.check()?;
            warnings.push("USB服务不可用；不会自动启动或配对");
            Vec::new()
        }
    };
    Ok(json!({"event":"inventory", "outputs":outputs, "devices":devices, "warnings":warnings}))
}
fn parse(args: &[String]) -> Result<(String, Settings, bool)> {
    let mode = args
        .first()
        .context("需要discover、create-output或stream子命令")?
        .clone();
    ensure!(
        ["discover", "create-output", "stream"].contains(&mode.as_str()),
        "无效子命令"
    );
    let mut settings = Settings::default();
    let mut confirmed = false;
    let mut i = 1;
    let mut seen = std::collections::HashSet::new();
    while i < args.len() {
        let key = &args[i];
        ensure!(seen.insert(key.clone()), "重复命令参数");
        match (mode.as_str(), key.as_str()) {
            ("create-output", "--confirm") => {
                confirmed = true;
                i += 1;
                continue;
            }
            ("stream", "--elevate") => {
                settings.elevate = true;
                i += 1;
                continue;
            }
            ("stream", "--socket-reuse-staging" | "--socket-fresh-staging") => {
                i += 1;
                continue;
            }
            ("stream", "--socket-writev") => {
                settings.socket_writev = true;
                i += 1;
                continue;
            }
            ("stream", "--socket-timing") => {
                settings.socket_timing = true;
                i += 1;
                continue;
            }
            ("stream", "--socket-skip-expired-wait" | "--socket-legacy-wait") => {
                settings.socket_skip_expired_wait = key == "--socket-skip-expired-wait";
                i += 1;
                continue;
            }
            ("stream", _) => {}
            _ => bail!("该子命令不接受此参数"),
        }
        let value = args.get(i + 1).context("命令参数缺少值")?;
        match key.as_str() {
            "--device" => settings.device = value.clone(),
            "--connector" => settings.connector = value.parse()?,
            "--device-id" => settings.device_id = value.parse()?,
            "--quality" => settings.quality = value.parse()?,
            "--fps" => settings.fps = value.parse()?,
            "--transport" => settings.transport = value.clone(),
            "--frames" => settings.frames = value.parse()?,
            "--seconds" => settings.seconds = value.parse()?,
            #[cfg(feature = "frame-diagnostics")]
            "--diagnostic-dir" => settings.diagnostic_dir = Some(PathBuf::from(value)),
            _ => bail!("不支持的参数"),
        }
        i += 2;
    }
    if mode == "create-output" {
        ensure!(confirmed, "创建输出需要--confirm");
    }
    if mode == "stream" {
        let legacy = seen.contains("--socket-legacy-wait");
        if legacy {
            ensure!(settings.transport == "socket", "旧限速仅支持普通 Socket");
            ensure!(!seen.contains("--socket-skip-expired-wait"), "限速参数互斥");
        }
        if settings.transport == "socket" && !legacy {
            settings.socket_skip_expired_wait = true;
        }
        let reuse = seen.contains("--socket-reuse-staging");
        let fresh = seen.contains("--socket-fresh-staging");
        ensure!(!(reuse && fresh), "暂存参数互斥");
        ensure!(
            !(reuse || fresh) || settings.transport == "socket",
            "暂存参数仅支持普通 Socket"
        );
        settings.socket_reuse_staging =
            settings.transport == "socket" && !fresh && (!settings.socket_writev || reuse);
        settings.validate()?;
    }
    Ok((mode, settings, confirmed))
}
fn parse_output_settings(args: &[String]) -> Result<(&str, u32, output::DisplayMode)> {
    let mut values = std::collections::HashMap::new();
    let mut confirmed = false;
    let mut args = args.iter();
    while let Some(key) = args.next().map(String::as_str) {
        match key {
            "--confirm" => {
                ensure!(!confirmed, "重复命令参数");
                confirmed = true;
            }
            "--device" | "--connector" | "--resolution" | "--scale" => {
                let value = args.next().context("命令参数缺少值")?;
                ensure!(values.insert(key, value.as_str()).is_none(), "重复命令参数");
            }
            _ => bail!("不支持的虚拟输出设置参数"),
        }
    }
    ensure!(confirmed, "调整输出需要--confirm");
    let device = *values.get("--device").context("缺少 DRM 节点")?;
    ensure!(valid_card(device), "必须选择明确的VKMS DRM节点");
    let connector = values
        .get("--connector")
        .context("缺少连接器 ID")?
        .parse()?;
    ensure!(connector > 0, "无效的连接器 ID");
    let mode = output::DisplayMode::parse(
        values.get("--resolution").context("缺少分辨率")?,
        values.get("--scale").context("缺少缩放倍率")?,
    )?;
    Ok((device, connector, mode))
}

async fn report_output(root: &Path, stop: &Stop, result: Value) -> Result<()> {
    emit(inventory(root, stop).await?);
    let mut fields = result.as_object().context("invalid output result")?.clone();
    fields.insert("event".into(), json!("output_ready"));
    emit(Value::Object(fields));
    Ok(())
}

async fn dispatch(args: &[String], stop: &Stop) -> Result<()> {
    // Never permit GTK/backend to run as root; only fixed native helper commands elevate.
    ensure!(unsafe { libc::geteuid() } != 0, "后端必须以普通用户运行");
    if args.first().is_some_and(|arg| arg == "auto") {
        let options = automation::Options::parse(&args[1..])?;
        return automation::run(&root()?, options, stop).await;
    }
    if args.first().is_some_and(|arg| arg == "configure-output") {
        let (device, connector, mode) = parse_output_settings(&args[1..])?;
        let root = root()?;
        let result = output::configure(&root, device, connector, mode, true, stop).await?;
        return report_output(&root, stop, result).await;
    }
    let (mode, settings, confirmed) = parse(args)?;
    let root = root()?;
    match mode.as_str() {
        "discover" => emit(inventory(&root, stop).await?),
        "create-output" => {
            let result = output::ensure(&root, confirmed, stop).await?;
            report_output(&root, stop, result).await?;
        }
        "stream" => session::stream(&root, &settings, stop).await?,
        _ => unreachable!(),
    }
    Ok(())
}
#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args == ["--help"] || args == ["-h"] {
        println!(
            "xdisplay-backend discover | create-output --confirm | configure-output --confirm --device /dev/dri/cardN --connector ID --resolution WIDTHxHEIGHT --scale SCALE | auto --confirm --resolution WIDTHxHEIGHT --scale SCALE [--elevate] | stream --device /dev/dri/cardN --connector ID --device-id ID [--quality 75|85|90 --fps 30|60 --transport socket|socket-direct --frames 1..300 --seconds 1..15 --elevate --socket-timing --socket-legacy-wait --socket-writev|--socket-fresh-staging (socket only)]"
        );
        return;
    }
    if args == ["--version"] {
        println!(
            "xdisplay-backend {} (Rust; JPEG-only)",
            env!("CARGO_PKG_VERSION")
        );
        return;
    }
    let stop = Stop::default();
    let signal_stop = stop.clone();
    let signals = tokio::spawn(async move {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM registration");
        let closed = async {
            while !STATUS_CLOSED.load(std::sync::atomic::Ordering::Relaxed) {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        };
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {}, _ = closed => {} }
        signal_stop.request();
    });
    let result = dispatch(&args, &stop).await;
    signals.abort();
    let _ = signals.await;
    match result {
        Ok(()) => {
            if stop.requested() {
                emit(json!({"event":"stopped"}));
            }
        }
        Err(error) if error.downcast_ref::<process::Cancelled>().is_some() => {
            emit(json!({"event":"stopped"}))
        }
        Err(error) => {
            // Cleanup failures remain errors even if the user requested cancellation.
            emit(
                json!({"event":"error", "message":error.to_string().chars().take(300).collect::<String>()}),
            );
            std::process::exit(1);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn arguments(extra: &[&str]) -> Vec<String> {
        [
            vec![
                "stream",
                "--device",
                "/dev/dri/card0",
                "--connector",
                "54",
                "--device-id",
                "1",
            ],
            extra.to_vec(),
        ]
        .concat()
        .into_iter()
        .map(str::to_string)
        .collect()
    }
    #[test]
    fn output_settings_require_exact_target_and_confirmation() {
        let valid = [
            "--confirm",
            "--device",
            "/dev/dri/card2",
            "--connector",
            "54",
            "--resolution",
            "2360x1640",
            "--scale",
            "1.5",
        ]
        .map(str::to_owned);
        let (device, connector, _) = parse_output_settings(&valid).unwrap();
        assert_eq!((device, connector), ("/dev/dri/card2", 54));
        assert!(parse_output_settings(&valid[1..]).is_err());
        for (index, value) in [
            (2, "/dev/dri/renderD128"),
            (4, "0"),
            (6, "4096x4096"),
            (8, "NaN"),
        ] {
            let mut bad = valid.clone();
            bad[index] = value.into();
            assert!(parse_output_settings(&bad).is_err());
        }
        let mut duplicate = valid.to_vec();
        duplicate.extend(["--scale".into(), "2".into()]);
        assert!(parse_output_settings(&duplicate).is_err());
    }

    #[test]
    fn socket_pacing_default_and_explicit_legacy() {
        let skip = |extra: &[&str]| parse(&arguments(extra)).unwrap().1.socket_skip_expired_wait;
        assert!(!skip(&[]));
        assert!(skip(&["--transport", "socket"]));
        assert!(skip(&[
            "--socket-skip-expired-wait",
            "--transport",
            "socket"
        ]));
        assert!(!skip(&["--socket-legacy-wait", "--transport", "socket"]));
        assert!(parse(&arguments(&["--socket-legacy-wait"])).is_err());
        assert!(parse(&arguments(&["--socket-skip-expired-wait"])).is_err());
        for options in [
            ["--socket-legacy-wait", "--socket-skip-expired-wait"],
            ["--socket-skip-expired-wait", "--socket-legacy-wait"],
        ] {
            assert!(
                parse(&arguments(&[
                    "--transport",
                    "socket",
                    options[0],
                    options[1]
                ]))
                .is_err()
            );
        }
    }

    #[test]
    fn strict_settings() {
        let reuse = "--socket-reuse-staging";
        let fresh = "--socket-fresh-staging";
        assert!(parse(&arguments(&[reuse])).is_err());
        assert!(parse(&arguments(&[fresh])).is_err());
        assert!(parse(&arguments(&["--transport", "socket", fresh, reuse])).is_err());
        assert!(
            !parse(&arguments(&["--transport", "socket", fresh]))
                .unwrap()
                .1
                .socket_reuse_staging
        );
        assert!(!parse(&arguments(&[])).unwrap().1.continuous);
        assert!(
            parse(&arguments(&["--transport", "socket"]))
                .unwrap()
                .1
                .socket_reuse_staging
        );
        assert!(
            parse(&arguments(&["--transport", "socket", reuse]))
                .unwrap()
                .1
                .socket_reuse_staging
        );
        assert!(
            parse(&arguments(&[
                "--transport",
                "socket",
                reuse,
                "--socket-writev"
            ]))
            .is_err()
        );
        assert!(parse(&arguments(&["--socket-writev"])).is_err());
        assert!(
            !parse(&arguments(&["--transport", "socket"]))
                .unwrap()
                .1
                .socket_writev
        );
        assert!(
            parse(&arguments(&["--transport", "socket", "--socket-writev"]))
                .unwrap()
                .1
                .socket_writev
        );
        assert!(!parse(&arguments(&[])).unwrap().1.socket_timing);
        assert!(parse(&arguments(&["--socket-timing"])).is_err());
        assert!(
            parse(&arguments(&["--transport", "socket", "--socket-timing"]))
                .unwrap()
                .1
                .socket_timing
        );
        assert!(
            parse(&arguments(&[
                "--transport",
                "socket",
                "--socket-timing",
                "--socket-timing"
            ]))
            .is_err()
        );
        for pair in [
            ["--quality", "95"],
            ["--frames", "301"],
            ["--seconds", "16"],
            ["--fps", "120"],
            ["--transport", "slot"],
            ["--device", "/dev/dri/renderD128"],
        ] {
            assert!(parse(&arguments(&pair)).is_err());
        }
        assert!(parse(&["create-output".into()]).is_err());
        assert!(parse(&["discover".into(), "--elevate".into()]).is_err());
        assert!(!valid_card("/dev/dri/card0;id"));
    }
}
