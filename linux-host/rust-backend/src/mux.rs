//! Bounded usbmuxd XML requests; successful Connect retains the raw socket.
use crate::process::Stop;
use anyhow::{Context, Result, bail, ensure};
use plist::{Dictionary, Value};
use std::{future::Future, io::Cursor, path::Path, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    time::timeout,
};

const SOCKET: &str = "/var/run/usbmuxd";
const MAX_PLIST: usize = 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(10);

async fn bounded<T>(
    stop: &Stop,
    duration: Duration,
    work: impl Future<Output = Result<T>>,
) -> Result<T> {
    stop.check()?;
    tokio::select! {
        biased;
        _ = stop.wait() => return Err(Stop::error()),
        result = timeout(duration, work) => result.context("usbmux operation timed out")?,
    }
}

fn encode(payload: Dictionary, tag: u32) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    Value::Dictionary(payload).to_writer_xml(&mut body)?;
    ensure!(body.len() <= MAX_PLIST, "oversized usbmux plist");
    let mut bytes = Vec::with_capacity(16 + body.len());
    for n in [(16 + body.len()) as u32, 1, 8, tag] {
        bytes.extend_from_slice(&n.to_le_bytes());
    }
    bytes.extend_from_slice(&body);
    Ok(bytes)
}

async fn read_response(stream: &mut UnixStream, tag: u32) -> Result<Dictionary> {
    let mut header = [0u8; 16];
    stream.read_exact(&mut header).await?;
    let word = |i| u32::from_le_bytes([header[i], header[i + 1], header[i + 2], header[i + 3]]);
    let length = word(0) as usize;
    ensure!(
        (16..=16 + MAX_PLIST).contains(&length),
        "invalid usbmux response length"
    );
    ensure!(
        word(4) == 1 && word(8) == 8 && word(12) == tag,
        "unexpected usbmux version/type/tag"
    );
    let mut body = vec![0; length - 16];
    // Never buffer/prefetch: bytes after this plist belong to the App tunnel.
    stream.read_exact(&mut body).await?;
    let value = Value::from_reader_xml(Cursor::new(body)).context("malformed usbmux XML plist")?;
    match value {
        Value::Dictionary(dictionary) => Ok(dictionary),
        _ => bail!("usbmux plist must be a dictionary"),
    }
}

async fn request(
    stream: &mut UnixStream,
    message: &str,
    mut fields: Dictionary,
) -> Result<Dictionary> {
    fields.insert("MessageType".into(), message.into());
    fields.insert(
        "ClientVersionString".into(),
        "xdisplay-linux-prototype".into(),
    );
    fields.insert("ProgName".into(), "xdisplay-linux-prototype".into());
    fields.insert("kLibUSBMuxVersion".into(), Value::Integer(3u32.into()));
    stream.write_all(&encode(fields, 1)?).await?;
    read_response(stream, 1).await
}

fn device_list(reply: &Dictionary) -> Result<Vec<serde_json::Value>> {
    let devices = reply
        .get("DeviceList")
        .and_then(Value::as_array)
        .context("missing usbmux DeviceList")?;
    ensure!(devices.len() <= 256, "oversized usbmux DeviceList");
    devices
        .iter()
        .map(|device| {
            let record = device
                .as_dictionary()
                .context("invalid usbmux device record")?;
            let id = record
                .get("DeviceID")
                .and_then(Value::as_unsigned_integer)
                .context("invalid usbmux DeviceID")?;
            ensure!(id > 0 && id <= u32::MAX as u64, "invalid usbmux DeviceID");
            let properties = match record.get("Properties") {
                Some(value) => Some(value.as_dictionary().context("invalid usbmux Properties")?),
                None => None,
            };
            let connection = properties
                .and_then(|p| p.get("ConnectionType"))
                .and_then(Value::as_string);
            // Deliberately retain no serials, UDIDs, or unfiltered properties.
            Ok(serde_json::json!({"device_id": id, "connection_type": connection}))
        })
        .collect()
}

async fn devices_at(path: &Path, stop: &Stop) -> Result<Vec<serde_json::Value>> {
    bounded(stop, DEADLINE, async {
        let mut stream = UnixStream::connect(path).await?;
        device_list(&request(&mut stream, "ListDevices", Dictionary::new()).await?)
    })
    .await
}

pub async fn devices(stop: &Stop) -> Result<Vec<serde_json::Value>> {
    devices_at(Path::new(SOCKET), stop).await
}

async fn connect_at(path: &Path, device_id: u32, stop: &Stop) -> Result<UnixStream> {
    ensure!(device_id != 0, "invalid usbmux DeviceID");
    bounded(stop, DEADLINE, async {
        let mut stream = UnixStream::connect(path).await?;
        let mut fields = Dictionary::new();
        fields.insert("DeviceID".into(), Value::Integer(device_id.into()));
        fields.insert(
            "PortNumber".into(),
            Value::Integer(u32::from(6789u16.to_be()).into()),
        );
        let reply = request(&mut stream, "Connect", fields).await?;
        ensure!(
            reply.get("MessageType").and_then(Value::as_string) == Some("Result")
                && reply.get("Number").and_then(Value::as_unsigned_integer) == Some(0),
            "usbmux Connect rejected"
        );
        Ok(stream)
    })
    .await
}

pub async fn connect(device_id: u32, stop: &Stop) -> Result<UnixStream> {
    connect_at(Path::new(SOCKET), device_id, stop).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::net::UnixListener;

    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn bind() -> (Self, UnixListener) {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "xdisplay-mux-{}-{}.sock",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let listener = UnixListener::bind(&path).unwrap();
            (Self(path), listener)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_file(&self.0).expect("remove owned fixture socket");
        }
    }
    fn dict(entries: &[(&str, Value)]) -> Dictionary {
        entries
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }
    fn result(code: u32) -> Dictionary {
        dict(&[
            ("MessageType", "Result".into()),
            ("Number", Value::Integer(code.into())),
        ])
    }

    #[tokio::test]
    async fn listing_projects_public_fields_only() {
        let (fixture, listener) = Fixture::bind();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_response(&mut stream, 1).await.unwrap();
            assert_eq!(
                request.get("MessageType").and_then(Value::as_string),
                Some("ListDevices")
            );
            let properties = dict(&[
                ("ConnectionType", "USB".into()),
                ("SerialNumber", "secret".into()),
            ]);
            let device = dict(&[
                ("DeviceID", Value::Integer(42u32.into())),
                ("Properties", Value::Dictionary(properties)),
            ]);
            let reply = dict(&[("DeviceList", Value::Array(vec![Value::Dictionary(device)]))]);
            stream.write_all(&encode(reply, 1).unwrap()).await.unwrap();
        });
        let output = devices_at(&fixture.0, &Stop::default()).await.unwrap();
        assert_eq!(
            output,
            vec![serde_json::json!({"device_id":42,"connection_type":"USB"})]
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn connect_keeps_app_bytes_after_response() {
        let (fixture, listener) = Fixture::bind();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_response(&mut stream, 1).await.unwrap();
            assert_eq!(
                request.get("MessageType").and_then(Value::as_string),
                Some("Connect")
            );
            assert_eq!(
                request.get("DeviceID").and_then(Value::as_unsigned_integer),
                Some(42)
            );
            assert_eq!(
                request
                    .get("PortNumber")
                    .and_then(Value::as_unsigned_integer),
                Some(u64::from(6789u16.to_be()))
            );
            let mut bytes = encode(result(0), 1).unwrap();
            bytes.extend_from_slice(&[4, 0, 0, 0]);
            stream.write_all(&bytes).await.unwrap();
        });
        let mut stream = connect_at(&fixture.0, 42, &Stop::default()).await.unwrap();
        let mut app = [0; 4];
        stream.read_exact(&mut app).await.unwrap();
        assert_eq!(app, [4, 0, 0, 0]);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn connect_rejection_and_stopped_before_io() {
        let (fixture, listener) = Fixture::bind();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_response(&mut stream, 1).await.unwrap();
            stream
                .write_all(&encode(result(3), 1).unwrap())
                .await
                .unwrap();
        });
        assert!(connect_at(&fixture.0, 1, &Stop::default()).await.is_err());
        server.await.unwrap();
        let stop = Stop::default();
        stop.request();
        assert!(devices_at(&fixture.0, &stop).await.is_err());
        assert!(connect_at(&fixture.0, 0, &Stop::default()).await.is_err());
    }

    #[tokio::test]
    async fn response_rejects_invalid_header_and_xml() {
        let good = encode(result(0), 1).unwrap();
        let mut variants = Vec::new();
        for (offset, value) in [
            (0, 15u32),
            (0, (16 + MAX_PLIST + 1) as u32),
            (4, 2),
            (8, 9),
            (12, 2),
        ] {
            let mut bytes = good.clone();
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            variants.push(bytes);
        }
        variants.push(good[..20].to_vec());
        let mut malformed = good.clone();
        malformed[16] = b'!';
        variants.push(malformed);
        let mut array = Vec::new();
        Value::Array(vec![]).to_writer_xml(&mut array).unwrap();
        let mut bytes = Vec::new();
        for n in [(16 + array.len()) as u32, 1, 8, 1] {
            bytes.extend_from_slice(&n.to_le_bytes());
        }
        bytes.extend_from_slice(&array);
        variants.push(bytes);
        for bytes in variants {
            let (mut reader, mut writer) = UnixStream::pair().unwrap();
            let sender = tokio::spawn(async move {
                writer.write_all(&bytes).await.unwrap();
            });
            assert!(read_response(&mut reader, 1).await.is_err());
            sender.await.unwrap();
        }
    }

    #[test]
    fn device_validation_does_not_hide_errors() {
        assert!(device_list(&Dictionary::new()).is_err());
        for value in [
            Value::String("1".into()),
            Value::Integer(0u32.into()),
            Value::Integer((u32::MAX as u64 + 1).into()),
        ] {
            let record = dict(&[("DeviceID", value)]);
            assert!(
                device_list(&dict(&[(
                    "DeviceList",
                    Value::Array(vec![Value::Dictionary(record)])
                )]))
                .is_err()
            );
        }
        let record = dict(&[("DeviceID", Value::Integer(1u32.into()))]);
        assert_eq!(
            device_list(&dict(&[(
                "DeviceList",
                Value::Array(vec![Value::Dictionary(record)])
            )]))
            .unwrap(),
            vec![serde_json::json!({"device_id":1,"connection_type":null})]
        );
    }

    #[tokio::test]
    async fn deadline_and_mid_read_cancellation() {
        let stop = Stop::default();
        assert!(
            bounded::<()>(&stop, Duration::from_millis(10), std::future::pending())
                .await
                .is_err()
        );
        let (fixture, listener) = Fixture::bind();
        let cancel = stop.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_response(&mut stream, 1).await.unwrap();
            cancel.request();
            let mut byte = [0];
            assert_eq!(stream.read(&mut byte).await.unwrap(), 0);
        });
        assert!(devices_at(&fixture.0, &stop).await.is_err());
        server.await.unwrap();
    }
}
