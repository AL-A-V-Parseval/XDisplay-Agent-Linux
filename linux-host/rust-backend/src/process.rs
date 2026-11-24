//! Owned child lifecycle; cancellation never abandons an unreaped process.
use anyhow::{Context, Result, bail};
use std::{
    path::Path,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
    time::{sleep, timeout},
};

#[derive(Debug)]
pub struct Cancelled;
impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "操作已停止")
    }
}
impl std::error::Error for Cancelled {}
#[derive(Clone, Default)]
pub struct Stop(Arc<AtomicBool>);
impl Stop {
    pub fn request(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn requested(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    pub fn error() -> anyhow::Error {
        anyhow::Error::new(Cancelled)
    }
    pub fn check(&self) -> Result<()> {
        if self.requested() {
            return Err(Self::error());
        }
        Ok(())
    }
    pub async fn wait(&self) {
        while !self.requested() {
            sleep(Duration::from_millis(20)).await;
        }
    }
}

pub async fn reap(child: &mut Child) -> Result<()> {
    child.stdin.take();
    if timeout(Duration::from_millis(300), child.wait())
        .await
        .is_ok()
    {
        // wait() errors must not be silently interpreted as successful cleanup.
        child.try_wait()?.context("child exit not observed")?;
        return Ok(());
    }
    if let Some(pid) = child.id() {
        // Child is still owned and unreaped; no PID lookup or process-group kill.
        let rc = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        if rc != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                // Already-authorized fixed root helper: never pretend it was stopped.
                child.wait().await?;
                return Ok(());
            }
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error.into());
            }
        }
    }
    if let Ok(status) = timeout(Duration::from_secs(2), child.wait()).await {
        status?;
        return Ok(());
    }
    match child.start_kill() {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            child.wait().await?;
            return Ok(());
        }
        Err(e) if e.raw_os_error() == Some(libc::ESRCH) => {}
        Err(e) => return Err(e.into()),
    }
    child.wait().await?;
    Ok(())
}

pub async fn run(exe: &Path, args: &[String], duration: Duration, stop: &Stop) -> Result<Vec<u8>> {
    stop.check()?;
    let mut child = Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| {
            format!(
                "无法启动 {}",
                exe.file_name().unwrap_or_default().to_string_lossy()
            )
        })?;
    let mut out = child.stdout.take().context("missing metadata stdout")?;
    let operation = async {
        let mut bytes = Vec::new();
        (&mut out).take(1_048_577).read_to_end(&mut bytes).await?;
        if bytes.len() > 1_048_576 {
            bail!("命令元数据超过大小限制");
        }
        let status = child.wait().await?;
        if !status.success() {
            bail!(
                "命令失败：{}",
                exe.file_name().unwrap_or_default().to_string_lossy()
            );
        }
        Ok(bytes)
    };
    let result = tokio::select! {
        value = timeout(duration, operation) => value.unwrap_or_else(|_| Err(anyhow::anyhow!("等待命令超时；已生效的设置不会自动撤销"))),
        _ = stop.wait() => Err(Stop::error()),
    };
    // No ? before reaping: a timeout/cancel/error follows the same ownership path.
    drop(out);
    reap(&mut child).await?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn command_success_and_failure() {
        assert_eq!(
            run(
                Path::new("/usr/bin/printf"),
                &["hello".into()],
                Duration::from_secs(2),
                &Stop::default()
            )
            .await
            .unwrap(),
            b"hello"
        );
        assert!(
            run(
                Path::new("/usr/bin/false"),
                &[],
                Duration::from_secs(2),
                &Stop::default()
            )
            .await
            .is_err()
        );
    }
    #[tokio::test]
    async fn cancellation_and_timeout_reap() {
        let stop = Stop::default();
        let cloned = stop.clone();
        let trigger = tokio::spawn(async move {
            sleep(Duration::from_millis(20)).await;
            cloned.request();
        });
        assert!(
            run(
                Path::new("/usr/bin/sleep"),
                &["20".into()],
                Duration::from_secs(30),
                &stop
            )
            .await
            .is_err()
        );
        trigger.await.unwrap();
        assert!(
            run(
                Path::new("/usr/bin/sleep"),
                &["20".into()],
                Duration::from_millis(20),
                &Stop::default()
            )
            .await
            .is_err()
        );
    }
}
