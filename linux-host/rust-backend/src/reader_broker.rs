//! Foreground automatic-session authorization. No listening socket or saved grant.
use crate::process::{Stop, reap};
use anyhow::{Context, Result, bail, ensure};
use std::{
    fs,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{fs::MetadataExt, net::UnixStream as LocalSocket},
    },
    path::Path,
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::Interest,
    net::UnixStream,
    process::{Child, Command},
    time::timeout,
};

pub(crate) const HELPER: &str = "/usr/lib/xdisplay-agent-linux/vkms-read";

pub(crate) fn validate_install(path: &Path) -> Result<()> {
    for (index, component) in path.ancestors().enumerate() {
        let meta = fs::symlink_metadata(component)
            .context("请先将专用 VKMS 读取器安装到系统目录（见 README）")?;
        ensure!(
            meta.uid() == 0
                && meta.mode() & 0o022 == 0
                && if index == 0 {
                    meta.is_file() && meta.mode() & 0o6111 == 0o111
                } else {
                    meta.is_dir()
                },
            "专用 VKMS 读取器及其上级目录必须由 root 所有，且不能由普通用户改写"
        );
    }
    Ok(())
}

fn control_pair() -> std::io::Result<(UnixStream, OwnedFd)> {
    let mut fds = [-1; 2];
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let (local, remote) = unsafe {
        (
            LocalSocket::from_raw_fd(fds[0]),
            OwnedFd::from_raw_fd(fds[1]),
        )
    };
    local.set_nonblocking(true)?;
    Ok((UnixStream::from_std(local)?, remote))
}

fn send_capture(
    socket: &UnixStream,
    capture: &LocalSocket,
    card: u32,
    connector: u32,
) -> std::io::Result<()> {
    let mut args = [0; 8];
    args[..4].copy_from_slice(&card.to_le_bytes());
    args[4..].copy_from_slice(&connector.to_le_bytes());
    let mut iov = libc::iovec {
        iov_base: args.as_mut_ptr().cast(),
        iov_len: args.len(),
    };
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    // Store ancillary data at native word alignment.
    let mut ancillary = [0usize; 8];
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = ancillary.as_mut_ptr().cast();
    let fd_size = std::mem::size_of::<libc::c_int>() as u32;
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(fd_size) } as usize;
    let sent = unsafe {
        let h = libc::CMSG_FIRSTHDR(&msg);
        (*h).cmsg_level = libc::SOL_SOCKET;
        (*h).cmsg_type = libc::SCM_RIGHTS;
        (*h).cmsg_len = libc::CMSG_LEN(fd_size) as usize;
        libc::CMSG_DATA(h)
            .cast::<libc::c_int>()
            .write_unaligned(capture.as_raw_fd());
        libc::sendmsg(
            socket.as_raw_fd(),
            &msg,
            libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
        )
    };
    if sent == args.len() as isize {
        Ok(())
    } else if sent < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Err(std::io::Error::other("short VKMS control message"))
    }
}

#[derive(Default)]
pub(crate) struct ReaderBroker {
    control: Option<UnixStream>,
    child: Option<Child>,
    ready: bool,
    active: bool,
}

async fn expect(socket: &UnixStream, expected: u8) -> Result<()> {
    loop {
        socket.readable().await?;
        let mut reply = [0; 2];
        match socket.try_read(&mut reply) {
            Ok(1) if reply[0] == expected => return Ok(()),
            Ok(_) => bail!("VKMS 授权助手已退出或响应无效"),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e.into()),
        }
    }
}

impl ReaderBroker {
    async fn start(&mut self, stop: &Stop) -> Result<()> {
        stop.check()?;
        validate_install(Path::new(HELPER))?;
        let (control, peer) = control_pair()?;
        self.child = Some(
            Command::new("/usr/bin/pkexec")
                .args([HELPER, "--broker-v1"])
                .stdin(Stdio::null())
                .stdout(Stdio::from(peer))
                .stderr(Stdio::null())
                .spawn()
                .context("无法启动 VKMS 授权助手")?,
        );
        self.control = Some(control);
        let result = tokio::select! {
            _ = stop.wait() => Err(Stop::error()),
            result = timeout(Duration::from_secs(60), expect(self.control.as_ref().unwrap(), b'R')) =>
                result.context("等待 VKMS 系统授权超时").and_then(|v| v),
        };
        if result.is_err() {
            self.close().await?;
        } else {
            self.ready = true;
        }
        result
    }

    pub(crate) async fn open(
        &mut self,
        device: &str,
        connector: u32,
        stop: &Stop,
    ) -> Result<UnixStream> {
        ensure!(!self.active, "上一次 VKMS 采集尚未清理");
        let card: u32 = device
            .strip_prefix("/dev/dri/card")
            .context("无效的 VKMS 设备")?
            .parse()?;
        ensure!(connector != 0, "无效的 VKMS 连接器");
        if self
            .child
            .as_mut()
            .map(|child| child.try_wait())
            .transpose()?
            .flatten()
            .is_some()
        {
            self.close().await?; // Ordinary idle expiration: obtain fresh authorization on demand.
        }
        if !self.ready {
            // A receiver disconnect can cancel the authorization future itself.
            // Retire that child instead of reusing an unacknowledged control channel.
            self.close().await?;
            self.start(stop).await?;
        }
        stop.check()?;
        let (local, peer) = LocalSocket::pair()?;
        local.set_nonblocking(true)?;
        let local = UnixStream::from_std(local)?;
        let control = self.control.as_ref().unwrap();
        // This fixed-size command is all-or-nothing on SOCK_SEQPACKET.
        let send = async {
            loop {
                control.writable().await?;
                match control.try_io(Interest::WRITABLE, || {
                    send_capture(control, &peer, card, connector)
                }) {
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                    result => return result,
                }
            }
        };
        tokio::select! {
            _ = stop.wait() => return Err(Stop::error()),
            sent = timeout(Duration::from_secs(2), send) => sent.context("发送 VKMS 采集请求超时")??,
        }
        self.active = true;
        Ok(local)
    }

    pub(crate) async fn capture_closed(&mut self) -> Result<()> {
        if !self.active {
            return if self.ready {
                Ok(())
            } else {
                self.close().await
            };
        }
        let result = timeout(
            Duration::from_secs(17),
            expect(self.control.as_ref().unwrap(), b'D'),
        )
        .await
        .context("等待 VKMS 采集资源释放超时")
        .and_then(|v| v);
        if result.is_err() {
            self.close().await?;
        }
        self.active = false;
        result
    }

    pub(crate) async fn close(&mut self) -> Result<()> {
        self.control.take();
        self.ready = false;
        self.active = false;
        if let Some(child) = self.child.as_mut() {
            reap(child).await?;
            self.child.take();
        }
        Ok(())
    }
}
