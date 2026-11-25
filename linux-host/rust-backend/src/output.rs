//! Temporary VKMS setup only: no pixels, physical outputs, or persistent config.
use crate::process::{Stop, run};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    ffi::CString,
    fs::{self, File},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::MetadataExt,
    },
    path::{Path, PathBuf},
    time::Duration,
};

const DRM_ROOT: &str = "/sys/class/drm";
const POLL_COUNT: usize = 20;
const NIRI: &str = "/usr/bin/niri";

fn card_name(name: &str) -> bool {
    name.strip_prefix("card")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

fn device_card(device: &str) -> Result<&str> {
    let name = device.strip_prefix("/dev/dri/").unwrap_or("");
    ensure!(card_name(name), "只允许 DRM card 节点");
    Ok(name)
}

fn parse_probe(device: &str, data: &[u8]) -> Result<Vec<Value>> {
    device_card(device)?;
    let text = std::str::from_utf8(data)?;
    let mut rows = Vec::new();
    for line in text.lines() {
        // The native probe also emits plane metadata; those are not connectors.
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let (Some(connector), Some(index), Some(active)) = (
            row["connector_id"].as_u64(),
            row["virtual_index"].as_u64(),
            row["active"].as_bool(),
        ) else {
            continue;
        };
        if row["driver"] != "vkms"
            || connector == 0
            || connector > u32::MAX as u64
            || !(1..=256).contains(&index)
        {
            continue;
        }
        let mut item =
            json!({"device":device,"connector":connector,"virtual_index":index,"active":active});
        if active {
            let (Some(width), Some(height)) = (row["width"].as_u64(), row["height"].as_u64())
            else {
                continue;
            };
            if !(16..=4096).contains(&width) || !(16..=4096).contains(&height) {
                continue;
            }
            item["width"] = json!(width);
            item["height"] = json!(height);
        }
        rows.push(item);
    }
    Ok(rows)
}

async fn command(exe: &Path, args: &[String], timeout: Duration, stop: &Stop) -> Result<Vec<u8>> {
    stop.check()?;
    let data = run(exe, args, timeout, stop).await?;
    stop.check()?;
    ensure!(data.len() <= 1_048_576, "虚拟输出元数据超过安全大小限制");
    Ok(data)
}

/// Read native probe metadata only. Sysfs driver names are merely a prefilter;
/// vkms-probe must independently accept the actual DRM driver and connector type.
pub async fn scan(root: &Path, stop: &Stop) -> Result<Vec<Value>> {
    stop.check()?;
    let mut cards = fs::read_dir(DRM_ROOT)?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    cards.sort();
    let mut rows = Vec::new();
    for card in cards {
        stop.check()?;
        let Some(name) = card
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| card_name(n))
        else {
            continue;
        };
        let driver = match fs::canonicalize(card.join("device/driver")) {
            Ok(driver) => driver,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if !matches!(
            driver.file_name().and_then(|s| s.to_str()),
            Some("vkms" | "faux_driver")
        ) {
            continue;
        }
        let device = format!("/dev/dri/{name}");
        let data = command(
            &root.join("vkms-probe"),
            &[device.clone()],
            Duration::from_secs(3),
            stop,
        )
        .await?;
        rows.extend(parse_probe(&device, &data)?);
    }
    Ok(rows)
}

fn connector_name(target: &Value) -> Result<String> {
    let index = target["virtual_index"]
        .as_u64()
        .context("无效的 VKMS 连接器索引")?;
    ensure!((1..=256).contains(&index), "无效的 VKMS 连接器索引");
    Ok(format!("Virtual-{index}"))
}

fn verify_connector_name(target: &Value, drm_root: &Path) -> Result<String> {
    let name = connector_name(target)?;
    let card = device_card(target["device"].as_str().context("无效的 DRM 设备")?)?;
    let suffix = format!("-{name}");
    let mut matches = Vec::new();
    for entry in fs::read_dir(drm_root)? {
        let entry = entry?;
        let filename = entry.file_name();
        let Some(filename) = filename.to_str() else {
            continue;
        };
        if filename.strip_suffix(&suffix).is_some_and(card_name) {
            matches.push(entry.path());
        }
    }
    ensure!(
        matches.len() == 1
            && matches[0].file_name().and_then(|s| s.to_str())
                == Some(format!("{card}-{name}").as_str()),
        "虚拟输出名称与 DRM 连接器无法唯一对应，拒绝更改显示配置"
    );
    let id: u64 = fs::read_to_string(matches[0].join("connector_id"))?
        .trim()
        .parse()?;
    ensure!(
        Some(id) == target["connector"].as_u64(),
        "虚拟输出连接器 ID 已改变，拒绝更改显示配置"
    );
    Ok(name)
}

async fn niri_outputs(stop: &Stop) -> Result<Value> {
    let args = ["msg", "-j", "outputs"].map(str::to_owned);
    let data = command(Path::new(NIRI), &args, Duration::from_secs(3), stop).await?;
    let outputs: Value = serde_json::from_slice(&data)?;
    ensure!(outputs.is_object(), "niri 输出元数据格式无效");
    Ok(outputs)
}

#[derive(Clone, Copy, Debug)]
pub struct DisplayMode {
    width: u32,
    height: u32,
    scale_percent: u32,
}

impl DisplayMode {
    const DEFAULT: Self = Self {
        width: 2360,
        height: 1640,
        scale_percent: 150,
    };

    pub fn parse(resolution: &str, scale: &str) -> Result<Self> {
        let (width, height) = match resolution {
            "1024x768" => (1024, 768),
            "1600x1200" => (1600, 1200),
            "1920x1080" => (1920, 1080),
            "2360x1640" => (2360, 1640),
            _ => bail!("不支持的虚拟屏分辨率"),
        };
        let scale_percent = match scale {
            "1" => 100,
            "1.25" => 125,
            "1.5" => 150,
            "1.75" => 175,
            "2" => 200,
            _ => bail!("不支持的虚拟屏缩放倍率"),
        };
        Ok(Self {
            width,
            height,
            scale_percent,
        })
    }

    fn scale(self) -> f64 {
        f64::from(self.scale_percent) / 100.0
    }

    fn actions(self) -> [Vec<String>; 2] {
        [
            vec![
                "custom-mode".into(),
                format!("{}x{}@60", self.width, self.height),
            ],
            vec!["scale".into(), self.scale().to_string()],
        ]
    }
}

fn mode_matches(row: &Value, requested: DisplayMode, normal: bool) -> bool {
    let Some(index) = row["current_mode"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
    else {
        return false;
    };
    let Some(mode) = row["modes"].as_array().and_then(|m| m.get(index)) else {
        return false;
    };
    mode["width"].as_u64() == Some(u64::from(requested.width))
        && mode["height"].as_u64() == Some(u64::from(requested.height))
        && mode["refresh_rate"]
            .as_u64()
            .is_some_and(|r| r.abs_diff(60000) <= 1000)
        && row["logical"]["scale"].as_f64() == Some(requested.scale())
        && (!normal || row["logical"]["transform"] == "Normal")
}

// Keep the fd (and flock) alive throughout command cancellation/reaping. Never
// unlink a lock: a different inode would allow concurrent creation to proceed.
struct CreationLock {
    _file: File,
}
impl Drop for CreationLock {
    fn drop(&mut self) {
        // Release even if a concurrent fork has temporarily inherited this CLOEXEC fd.
        unsafe { libc::flock(self._file.as_raw_fd(), libc::LOCK_UN) };
    }
}
impl CreationLock {
    fn acquire() -> Result<Self> {
        let uid = unsafe { libc::getuid() };
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("/run/user/{uid}")));
        Self::in_runtime(&runtime, uid)
    }

    fn in_runtime(runtime: &Path, uid: u32) -> Result<Self> {
        use std::os::unix::ffi::OsStrExt;
        ensure!(runtime.is_absolute(), "运行目录必须为绝对路径");
        let path = CString::new(runtime.as_os_str().as_bytes())?;
        let fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .context("无法验证当前用户运行目录，未执行创建操作");
        }
        let directory = unsafe { File::from_raw_fd(fd) };
        let info = directory.metadata()?;
        ensure!(
            info.is_dir() && info.uid() == uid && info.mode() & 0o022 == 0,
            "无法验证当前用户运行目录，未执行创建操作"
        );
        let filename = CString::new("wired-xdisplay-vkms-create.lock")?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                filename.as_ptr(),
                libc::O_RDWR
                    | libc::O_CREAT
                    | libc::O_NOFOLLOW
                    | libc::O_CLOEXEC
                    | libc::O_NONBLOCK,
                0o600,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("无法安全打开 VKMS 创建锁");
        }
        let file = unsafe { File::from_raw_fd(fd) };
        let info = file.metadata()?;
        ensure!(
            info.is_file() && info.uid() == uid && info.nlink() == 1 && info.mode() & 0o077 == 0,
            "VKMS 创建锁不是当前用户的私有普通文件"
        );
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("另一个面板正在创建虚拟输出，或无法获取创建锁");
        }
        Ok(Self { _file: file })
    }
}

fn phase(message: &str) {
    crate::emit(json!({"event":"output_setup","message":message}));
}

async fn observation_delay(stop: &Stop) -> Result<()> {
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_millis(250)) => {},
        _ = stop.wait() => {},
    }
    stop.check()
}

fn changed(mut row: Value, value: bool) -> Value {
    row["changed"] = json!(value);
    row
}

#[derive(Debug)]
pub struct OwnedOutput {
    target: Value,
    identity: PathBuf,
}

fn connector_identity(target: &Value) -> Result<PathBuf> {
    let drm = Path::new(DRM_ROOT);
    let name = verify_connector_name(target, drm)?;
    let card = device_card(target["device"].as_str().context("缺少 DRM 节点")?)?;
    Ok(fs::canonicalize(drm.join(format!("{card}-{name}")))?)
}

impl OwnedOutput {
    pub fn from_parts(device: &str, connector: u32, index: u32, identity: &str) -> Result<Self> {
        device_card(device)?;
        ensure!(
            connector != 0 && (1..=256).contains(&index),
            "无效的 VKMS 连接器"
        );
        let identity = PathBuf::from(identity);
        ensure!(identity.starts_with("/sys/devices"), "无效的 VKMS 路径");
        Ok(Self {
            target: json!({"device":device,"connector":connector,"virtual_index":index}),
            identity,
        })
    }

    pub fn notice(&self) -> Value {
        json!({"event":"output_owned", "device":self.target["device"],
            "connector":self.target["connector"], "virtual_index":self.target["virtual_index"],
            "identity":self.identity})
    }

    /// Called with a fresh cancellation token so teardown still runs after Stop.
    pub async fn disable(self, root: &Path) -> Result<()> {
        let stop = Stop::default();
        let _lock = CreationLock::acquire()?;
        let same_target = |row: &Value| {
            ["device", "connector", "virtual_index"]
                .iter()
                .all(|key| row[*key] == self.target[*key])
        };
        if !scan(root, &stop).await?.iter().any(same_target) {
            ensure!(
                !self.identity.try_exists()?,
                "无法再验证本程序启用的 VKMS 输出"
            );
            return Ok(());
        }
        ensure!(
            connector_identity(&self.target)? == self.identity,
            "VKMS 连接器身份已改变"
        );
        let name = connector_name(&self.target)?;
        let outputs = niri_outputs(&stop).await?;
        if !outputs[&name]["current_mode"].is_u64() {
            return Ok(());
        }
        ensure!(
            has_other_active_output(&outputs, &name),
            "拒绝关闭桌面上最后一个启用的显示器"
        );
        ensure!(
            connector_identity(&self.target)? == self.identity,
            "VKMS 连接器身份已改变"
        );
        let args = ["msg".into(), "output".into(), name.clone(), "off".into()];
        command(Path::new(NIRI), &args, Duration::from_secs(3), &stop).await?;
        for _ in 0..POLL_COUNT {
            if !niri_outputs(&stop).await?[&name]["current_mode"].is_u64() {
                return Ok(());
            }
            observation_delay(&stop).await?;
        }
        bail!("未确认本程序启用的 VKMS 输出已关闭")
    }
}

#[cfg(test)]
#[test]
fn cleanup_requires_another_enabled_desktop_output() {
    let virtual_only = json!({"Virtual-1":{"current_mode":0,"logical":{}},
                              "eDP-1":{"current_mode":null,"logical":null}});
    assert!(!has_other_active_output(&virtual_only, "Virtual-1"));
    let mut both = virtual_only;
    both["eDP-1"] = json!({"current_mode": 0, "logical": {}});
    assert!(has_other_active_output(&both, "Virtual-1"));
    assert!(OwnedOutput::from_parts("/dev/dri/card0", 0, 1, "/sys/devices/test").is_err());
    assert!(OwnedOutput::from_parts("/dev/dri/card0", 1, 1, "/tmp/test").is_err());
}

fn has_other_active_output(outputs: &Value, name: &str) -> bool {
    outputs.as_object().is_some_and(|rows| {
        rows.iter().any(|(key, row)| {
            key != name && row["current_mode"].is_u64() && row["logical"].is_object()
        })
    })
}

pub async fn ensure_mode(
    root: &Path,
    confirmed: bool,
    mode: Option<DisplayMode>,
    stop: &Stop,
    on_owned: impl FnOnce(OwnedOutput),
) -> Result<Value> {
    ensure!(confirmed, "必须先明确确认 VKMS 创建操作");
    stop.check()?;
    let _lock = CreationLock::acquire()?;
    phase("正在检查现有 VKMS 输出…");
    let rows = scan(root, stop).await?;
    if mode.is_some() {
        ensure!(rows.len() <= 1, "检测到多个 VKMS 连接器，拒绝自动选择");
    }
    if let Some(active) = rows.iter().find(|r| r["active"] == true) {
        if let Some(mode) = mode {
            let name = verify_connector_name(active, Path::new(DRM_ROOT))?;
            if !mode_matches(&niri_outputs(stop).await?[&name], mode, true)
                || active["width"] != mode.width
                || active["height"] != mode.height
            {
                return apply_mode(root, active, mode, true, stop).await;
            }
        }
        return Ok(changed(active.clone(), false));
    }
    // Fail before requesting privilege if ordinary-user compositor IPC fails.
    niri_outputs(stop).await?;
    let loaded_here = rows.is_empty() && !Path::new("/sys/module/vkms").try_exists()?;
    if loaded_here {
        phase("请在系统授权窗口允许加载 VKMS；面板不会以 root 运行…");
        let args = ["/usr/bin/modprobe", "vkms", "enable_cursor=1"].map(str::to_owned);
        command(
            Path::new("/usr/bin/pkexec"),
            &args,
            Duration::from_secs(90),
            stop,
        )
        .await
        .context("VKMS 模块加载失败或授权被取消；已加载的模块不会自动卸载")?;
    }
    phase("等待 niri 识别 VKMS 虚拟连接器…");
    let mut selected = None;
    for _ in 0..POLL_COUNT {
        let rows = scan(root, stop).await?;
        ensure!(
            rows.len() <= 1,
            "检测到多个未选定的 VKMS 连接器，拒绝自动更改显示模式"
        );
        if let Some(target) = rows.into_iter().next() {
            let name = connector_name(&target)?;
            if niri_outputs(stop).await?.get(&name).is_some() {
                selected = Some(target);
                break;
            }
        }
        observation_delay(stop).await?;
    }
    let target = selected
        .context("niri 尚未识别对应 VKMS 输出；模块若已加载会保留，请检查设备权限或重启会话")?;
    let name = verify_connector_name(&target, Path::new(DRM_ROOT))?;
    let outputs = niri_outputs(stop).await?;
    ensure!(outputs.get(&name).is_some(), "VKMS 输出已消失");
    if loaded_here || outputs[&name]["current_mode"].is_null() {
        on_owned(OwnedOutput {
            identity: connector_identity(&target)?,
            target: target.clone(),
        });
    }
    apply_mode(
        root,
        &target,
        mode.unwrap_or(DisplayMode::DEFAULT),
        true,
        stop,
    )
    .await
}

/// Configure only the explicitly selected, already active VKMS connector.
pub async fn configure(
    root: &Path,
    device: &str,
    connector: u32,
    mode: DisplayMode,
    confirmed: bool,
    stop: &Stop,
) -> Result<Value> {
    ensure!(confirmed, "必须先明确确认虚拟输出设置");
    device_card(device)?;
    ensure!(connector > 0, "无效的连接器 ID");
    stop.check()?;
    let _lock = CreationLock::acquire()?;
    let target = select_active(scan(root, stop).await?, device, connector)?;
    let name = verify_connector_name(&target, Path::new(DRM_ROOT))?;
    ensure!(
        niri_outputs(stop).await?[&name]["current_mode"].is_u64(),
        "niri 中没有启用该虚拟输出，请重新检查设备"
    );
    apply_mode(root, &target, mode, false, stop).await
}

fn select_active(rows: Vec<Value>, device: &str, connector: u32) -> Result<Value> {
    let mut matches = rows
        .into_iter()
        .filter(|r| r["device"] == device && r["connector"] == connector && r["active"] == true);
    let target = matches
        .next()
        .context("选定的 VKMS 输出已失效，请重新检查设备")?;
    ensure!(matches.next().is_none(), "选定的 VKMS 输出不唯一");
    Ok(target)
}

async fn apply_mode(
    root: &Path,
    target: &Value,
    mode: DisplayMode,
    enable: bool,
    stop: &Stop,
) -> Result<Value> {
    phase(&format!(
        "正在{}虚拟输出：{}×{} / 60 Hz / 缩放 {}…",
        if enable { "启用" } else { "调整" },
        mode.width,
        mode.height,
        mode.scale()
    ));
    let mut actions = Vec::new();
    if enable {
        actions.push(vec!["on".into()]);
    }
    actions.extend(mode.actions());
    if enable {
        actions.push(vec!["transform".into(), "normal".into()]);
    }
    for action in actions {
        stop.check()?;
        let name = verify_connector_name(target, Path::new(DRM_ROOT))?;
        let mut args = vec!["msg".to_owned(), "output".to_owned(), name];
        args.extend(action);
        command(Path::new(NIRI), &args, Duration::from_secs(3), stop)
            .await
            .context("虚拟输出命令未完成；已执行的显示操作不会自动撤销")?;
    }
    let name = verify_connector_name(target, Path::new(DRM_ROOT))?;
    for _ in 0..POLL_COUNT {
        let rows = scan(root, stop).await?;
        let verified = rows.into_iter().find(|r| {
            r["device"] == target["device"]
                && r["connector"] == target["connector"]
                && r["active"] == true
                && r["width"] == mode.width
                && r["height"] == mode.height
        });
        let outputs = niri_outputs(stop).await?;
        if let Some(row) = verified {
            if mode_matches(&outputs[&name], mode, enable) {
                let mut result = changed(row, true);
                result["scale"] = json!(mode.scale());
                result["configured"] = json!(!enable);
                return Ok(result);
            }
        }
        observation_delay(stop).await?;
    }
    bail!("命令已执行，但未确认 VKMS 模式生效；输出可能仍启用，请重新检查设备，不会自动卸载模块")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(flavor = "current_thread")]
    async fn confirmation_is_required_before_any_io() {
        let error = ensure_mode(
            Path::new("/does-not-exist"),
            false,
            None,
            &Stop::default(),
            |_| panic!("no ownership without consent"),
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "必须先明确确认 VKMS 创建操作");
    }

    #[test]
    fn requested_mode_and_active_identity_are_verified() {
        let wanted = DisplayMode::parse("1920x1080", "1.25").unwrap();
        assert_eq!(
            wanted.actions(),
            [vec!["custom-mode", "1920x1080@60"], vec!["scale", "1.25"]]
        );
        let mut row = json!({"current_mode":0,"modes":[{"width":1920,"height":1080,"refresh_rate":59937}],
            "logical":{"scale":1.25,"transform":"90"}});
        assert!(mode_matches(&row, wanted, false));
        assert!(!mode_matches(&row, wanted, true));
        row["logical"]["scale"] = json!(1.5);
        assert!(!mode_matches(&row, wanted, false));
        let target = json!({"device":"/dev/dri/card2","connector":54,"active":true});
        assert_eq!(
            select_active(vec![target.clone()], "/dev/dri/card2", 54).unwrap(),
            target
        );
        assert!(select_active(vec![target.clone()], "/dev/dri/card0", 54).is_err());
        assert!(select_active(vec![target.clone(), target], "/dev/dri/card2", 54).is_err());
    }

    #[test]
    fn active_reuse_preserves_mode_metadata() {
        let row = json!({"device":"/dev/dri/card0","connector":35,"virtual_index":1,
            "active":true,"width":1920,"height":1080});
        let result = changed(row.clone(), false);
        assert_eq!(result["width"], row["width"]);
        assert_eq!(result["height"], row["height"]);
        assert_eq!(result["changed"], false);
    }

    #[test]
    fn strict_probe_metadata() {
        let input = br#"{"driver":"vkms","connector_id":35,"virtual_index":1,"active":true,"width":2360,"height":1640}
{"driver":"i915","connector_id":36,"virtual_index":1,"active":true,"width":2360,"height":1640}
{"driver":"vkms","connector_id":true,"virtual_index":1,"active":false}
{"driver":"vkms","connector_id":35,"virtual_index":257,"active":false}
{"driver":"vkms","connector_id":35,"virtual_index":1,"active":true,"width":0,"height":1640}
{"virtual_crtc_id":3,"plane_id":4}
{"driver":"vkms","connector_id":37,"virtual_index":2,"active":false}
not json"#;
        let rows = parse_probe("/dev/dri/card0", input).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["width"], 2360);
        assert!(rows[1].get("width").is_none());
        for device in [
            "/dev/dri/card",
            "/dev/dri/card0/../card1",
            "/dev/dri/renderD128",
        ] {
            assert!(parse_probe(device, input).is_err());
        }
    }

    // Fixtures only; never open DRM nodes or execute commands.
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            use std::os::unix::fs::PermissionsExt;
            let path = std::env::temp_dir().join(format!(
                "wired-output-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            // Only the exact unique directory created by this fixture is removed.
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn connector_aliases_and_id_mismatch_are_rejected() {
        let fixture = Fixture::new();
        let connector = fixture.0.join("card0-Virtual-1");
        fs::create_dir(&connector).unwrap();
        fs::write(connector.join("connector_id"), "35\n").unwrap();
        let target = json!({"device":"/dev/dri/card0","connector":35,"virtual_index":1});
        assert_eq!(
            verify_connector_name(&target, &fixture.0).unwrap(),
            "Virtual-1"
        );
        let alias = fixture.0.join("card1-Virtual-1");
        fs::create_dir(&alias).unwrap();
        assert!(verify_connector_name(&target, &fixture.0).is_err());
        fs::remove_dir(&alias).unwrap();
        fs::write(connector.join("connector_id"), "36\n").unwrap();
        assert!(verify_connector_name(&target, &fixture.0).is_err());
    }

    #[test]
    fn creation_lock_is_exclusive_released_and_never_unlinked() {
        let fixture = Fixture::new();
        let uid = unsafe { libc::getuid() };
        let first = CreationLock::in_runtime(&fixture.0, uid).unwrap();
        assert!(CreationLock::in_runtime(&fixture.0, uid).is_err());
        let path = fixture.0.join("wired-xdisplay-vkms-create.lock");
        let inode = fs::metadata(&path).unwrap().ino();
        drop(first);
        let second = CreationLock::in_runtime(&fixture.0, uid).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
        drop(second);
    }

    #[test]
    fn lock_and_runtime_symlinks_are_rejected() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let uid = unsafe { libc::getuid() };
        let real = fixture.0.join("real");
        fs::create_dir(&real).unwrap();
        let alias = fixture.0.join("alias");
        symlink(&real, &alias).unwrap();
        assert!(CreationLock::in_runtime(&alias, uid).is_err());
        let target = fixture.0.join("target");
        fs::write(&target, "untouched").unwrap();
        symlink(&target, fixture.0.join("wired-xdisplay-vkms-create.lock")).unwrap();
        assert!(CreationLock::in_runtime(&fixture.0, uid).is_err());
        assert_eq!(fs::read_to_string(target).unwrap(), "untouched");
    }

    #[test]
    fn compositor_mode_is_strict() {
        let mut row = json!({"current_mode":0,"modes":[{"width":2360,"height":1640,"refresh_rate":60000}],
            "logical":{"scale":1.5,"transform":"Normal"}});
        assert!(mode_matches(&row, DisplayMode::DEFAULT, true));
        row["current_mode"] = json!(true);
        assert!(!mode_matches(&row, DisplayMode::DEFAULT, true));
        row["current_mode"] = json!(1);
        assert!(!mode_matches(&row, DisplayMode::DEFAULT, true));
        row["current_mode"] = json!(0);
        row["logical"]["scale"] = json!(1);
        assert!(!mode_matches(&row, DisplayMode::DEFAULT, true));
    }
}
