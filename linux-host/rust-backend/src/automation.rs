//! Explicitly armed, single-device USB supervision. No pixel capture while waiting.
use crate::{Settings, emit, mux, output, process::Stop, session};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

pub struct Options {
    mode: output::DisplayMode,
    elevate: bool,
}

impl Options {
    pub fn parse(args: &[String]) -> Result<Self> {
        let (mut confirm, mut elevate, mut resolution, mut scale) = (false, false, None, None);
        let mut args = args.iter().map(String::as_str);
        while let Some(key) = args.next() {
            match key {
                "--confirm" if !confirm => confirm = true,
                "--elevate" if !elevate => elevate = true,
                "--resolution" if resolution.is_none() => {
                    resolution = Some(args.next().context("缺少分辨率")?)
                }
                "--scale" if scale.is_none() => scale = Some(args.next().context("缺少缩放倍率")?),
                _ => bail!("自动连接参数不支持或重复"),
            }
        }
        ensure!(confirm, "启用自动连接需要 --confirm");
        let mode = output::DisplayMode::parse(
            resolution.context("缺少分辨率")?,
            scale.context("缺少缩放倍率")?,
        )?;
        Ok(Self { mode, elevate })
    }
}

fn only_usb(rows: &[Value]) -> Option<u32> {
    let mut usb = rows.iter().filter(|v| v["connection_type"] == "USB");
    let id = usb.next()?["device_id"].as_u64()?;
    if usb.next().is_some() {
        return None;
    }
    u32::try_from(id).ok().filter(|id| *id != 0)
}

async fn delay(stop: &Stop, seconds: u64) -> Result<()> {
    tokio::select! {
        _ = stop.wait() => Err(Stop::error()),
        _ = tokio::time::sleep(Duration::from_secs(seconds)) => Ok(()),
    }
}

fn phase(name: &str) {
    emit(json!({"event":"phase", "phase":name}));
}
fn warning(error: &anyhow::Error) {
    emit(
        json!({"event":"auto_warning", "message":error.to_string().chars().take(300).collect::<String>()}),
    );
}

pub async fn run(root: &Path, options: Options, stop: &Stop) -> Result<()> {
    let mut previous = "";
    loop {
        stop.check()?;
        let devices = mux::devices(stop).await;
        stop.check()?;
        let id = match devices.as_deref().ok().and_then(only_usb) {
            Some(id) => id,
            None => {
                if previous != "waiting_usb" {
                    phase("waiting_usb");
                    previous = "waiting_usb";
                }
                delay(stop, 2).await?;
                continue;
            }
        };
        if previous != "waiting_app" {
            phase("waiting_app");
            previous = "waiting_app";
        }
        let Ok(tunnel) = mux::connect(id, stop).await else {
            delay(stop, 3).await?;
            continue;
        };
        stop.check()?;
        phase("configuring");
        // Revalidate even on the same USB ID: the compositor or VKMS may have restarted.
        let mut target = output::ensure_mode(root, true, Some(options.mode), stop).await?;
        target["event"] = json!("output_ready");
        emit(target.clone());
        let settings = Settings {
            device: target["device"]
                .as_str()
                .context("missing VKMS device")?
                .into(),
            connector: u32::try_from(target["connector"].as_u64().context("missing connector")?)?,
            device_id: id,
            transport: "socket".into(),
            socket_reuse_staging: true,
            socket_skip_expired_wait: true,
            elevate: options.elevate,
            continuous: true,
            ..Settings::default()
        };
        settings.validate()?;
        if let Err(error) = session::stream_with_tunnel(root, &settings, stop, Some(tunnel)).await {
            if error.is::<crate::process::Cancelled>() || error.is::<session::SourceFailure>() {
                return Err(error);
            }
            warning(&error);
        }
        previous = "";
        delay(stop, 4).await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn auto_requires_consent_and_fixed_mode_arguments() {
        let parse =
            |args: &[&str]| Options::parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let args = ["--resolution", "2360x1640", "--scale", "1.5"];
        assert!(parse(&args).is_err());
        assert!(
            parse(&[&args[..], &["--confirm", "--elevate"]].concat())
                .unwrap()
                .elevate
        );
        assert!(
            parse(&[&args[..], &["--confirm", "--transport", "socket-direct"]].concat()).is_err()
        );
    }
    #[test]
    fn only_single_usb_device_is_selected() {
        let usb = json!({"device_id":1,"connection_type":"USB"});
        assert_eq!(
            only_usb(&[json!({"connection_type":"Network"}), usb.clone()]),
            Some(1)
        );
        assert_eq!(only_usb(&[usb.clone(), usb]), None);
        assert_eq!(only_usb(&[]), None);
    }
}
