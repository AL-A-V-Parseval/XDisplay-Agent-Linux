//! JPEG-only XDisplay wire framing and input-observing state machine.
use anyhow::{Result, bail, ensure};
use tokio::io::{AsyncRead, AsyncReadExt};

pub const MAX_BODY: usize = 8 * 1024 * 1024;

#[derive(Default, Debug)]
pub struct Protocol {
    pub big: bool,
    pub size: Option<(u32, u32)>,
    pub streaming: bool,
}

/// Outbound framing uses the tag family, independently of payload byte order.
pub fn header(tag: u8, len: usize) -> Result<[u8; 4]> {
    ensure!(len <= MAX_BODY, "oversized XDisplay body");
    let n = len as u32;
    Ok(if tag >= 5 {
        [tag, (n >> 16) as u8, (n >> 8) as u8, n as u8]
    } else {
        [tag, n as u8, (n >> 8) as u8, (n >> 16) as u8]
    })
}

pub fn packet(tag: u8, body: &[u8]) -> Result<Vec<u8>> {
    let h = header(tag, body.len())?;
    let mut bytes = Vec::with_capacity(4 + body.len());
    bytes.extend_from_slice(&h);
    bytes.extend_from_slice(body);
    Ok(bytes)
}

pub async fn read_packet<R: AsyncRead + Unpin>(
    r: &mut R,
    big: Option<bool>,
) -> Result<(u8, Vec<u8>)> {
    let mut h = [0u8; 4];
    r.read_exact(&mut h).await?;
    let len = if big.unwrap_or(h[0] >= 5) {
        u32::from_be_bytes([0, h[1], h[2], h[3]])
    } else {
        u32::from_le_bytes([h[1], h[2], h[3], 0])
    } as usize;
    ensure!(len <= MAX_BODY, "XDisplay body exceeds safety limit");
    let mut body = vec![0; len];
    r.read_exact(&mut body).await?;
    Ok((h[0], body))
}

fn size(width: u32, height: u32) -> Result<()> {
    ensure!(
        (16..=4096).contains(&width)
            && (16..=4096).contains(&height)
            && u64::from(width) * u64::from(height) <= 8_388_608,
        "unsupported/suspicious dimensions {width}x{height}"
    );
    Ok(())
}

impl Protocol {
    fn command(&self, command: u32) -> Vec<u8> {
        let mut packet = Vec::with_capacity(16);
        packet.extend_from_slice(&[if self.big { 5 } else { 1 }, 12, 0, 0]);
        if self.big {
            packet[1..4].copy_from_slice(&[0, 0, 12]);
        }
        packet.extend_from_slice(&if self.big {
            command.to_be_bytes()
        } else {
            command.to_le_bytes()
        });
        packet.extend_from_slice(&[0; 8]);
        packet
    }

    pub fn hello(&self) -> Vec<u8> {
        self.command(1)
    }

    fn words(&self, body: &[u8], minimum: usize) -> Result<Vec<u32>> {
        ensure!(
            body.len() >= minimum * 4 && body.len() % 4 == 0,
            "truncated or unaligned word payload"
        );
        Ok(body
            .chunks_exact(4)
            .map(|word| {
                let bytes = [word[0], word[1], word[2], word[3]];
                if self.big {
                    u32::from_be_bytes(bytes)
                } else {
                    u32::from_le_bytes(bytes)
                }
            })
            .collect())
    }

    pub fn handle(&mut self, tag: u8, body: &[u8]) -> Result<Vec<Vec<u8>>> {
        ensure!(body.len() <= MAX_BODY, "oversized XDisplay body");
        match tag {
            4 => {
                ensure!(body.is_empty(), "byte-order marker must have empty body");
                ensure!(!self.streaming, "late byte-order switch not supported");
                self.big = true;
            }
            1 | 5 => {
                let words = self.words(body, 3)?;
                match words[0] {
                    2 => {
                        ensure!(
                            !self.streaming,
                            "handshake reconfiguration during streaming"
                        );
                        size(words[1], words[2])?;
                        self.size = Some((words[1], words[2]));
                        return Ok(vec![self.command(254)]);
                    }
                    255 => {
                        ensure!(!self.streaming, "duplicate stream start");
                        ensure!(self.size.is_some(), "stream start before dimensions");
                        self.streaming = true;
                    }
                    _ => return Ok(vec![self.command(253)]),
                }
            }
            3 | 7 => {
                let words = self.words(body, 1)?;
                match words[0] {
                    4..=15 => ensure!(words.len() >= 3, "short mouse control"),
                    16 | 17 => ensure!(
                        words.len() == 3,
                        "raw touch control must contain exactly three words"
                    ),
                    0x70 => {
                        ensure!(words.len() >= 3, "short resolution control");
                        size(words[1], words[2])?;
                        ensure!(
                            self.size == Some((words[1], words[2])),
                            "live resolution changes not implemented"
                        );
                    }
                    0x71 => {
                        ensure!(words.len() >= 2, "short codec control");
                        ensure!(words[1] == 0, "H.264/unknown codec requested; JPEG only");
                    }
                    _ => {} // Unknown numeric controls are observed but never injected.
                }
                // Recognized input is validated, then discarded: no injection path.
            }
            _ => bail!("unsupported incoming tag {tag}"),
        }
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(big: bool, values: &[u32]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|n| {
                if big {
                    n.to_be_bytes()
                } else {
                    n.to_le_bytes()
                }
            })
            .collect()
    }

    #[test]
    fn jpeg_handshake_both_orders() {
        for big in [false, true] {
            let mut p = Protocol::default();
            assert_eq!(
                p.hello(),
                vec![1, 12, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
            );
            if big {
                p.handle(4, &[]).unwrap();
            }
            let response = p
                .handle(if big { 5 } else { 1 }, &words(big, &[2, 1920, 1080, 9]))
                .unwrap();
            assert_eq!(response, vec![p.command(254)]);
            assert_eq!(p.size, Some((1920, 1080)));
            p.handle(1, &words(big, &[255, 0, 0])).unwrap();
            assert!(p.streaming);
            assert!(p.handle(4, &[]).is_err());
            assert!(p.handle(1, &words(big, &[255, 0, 0])).is_err());
            assert!(p.handle(1, &words(big, &[2, 640, 480])).is_err());
            assert!(p.handle(3, &words(big, &[0x71, 1])).is_err());
            assert!(p.handle(3, &words(big, &[0x71, 0])).unwrap().is_empty());
            p.handle(3, &words(big, &[0x70, 1920, 1080])).unwrap();
            assert!(p.handle(3, &words(big, &[0x70, 640, 480])).is_err());
            for input in [4, 15, 16, 17] {
                assert!(
                    p.handle(3, &words(big, &[input, u32::MAX, u32::MAX]))
                        .unwrap()
                        .is_empty()
                );
            }
        }
    }

    #[test]
    fn rejects_bad_states_and_payloads() {
        let mut p = Protocol::default();
        assert!(p.handle(1, &words(false, &[255, 0, 0])).is_err());
        for dims in [(0, 480), (4097, 16), (4096, 4096)] {
            assert!(p.handle(1, &words(false, &[2, dims.0, dims.1])).is_err());
        }
        for (tag, body) in [
            (4, vec![0]),
            (0, vec![]),
            (0x20, vec![]),
            (1, vec![0; 11]),
            (3, words(false, &[4, 0])),
            (7, words(false, &[16, 0, 0, 0])),
            (3, words(false, &[0x71])),
        ] {
            assert!(p.handle(tag, &body).is_err());
        }
        assert_eq!(
            p.handle(1, &words(false, &[99, 0, 0])).unwrap(),
            vec![p.command(253)]
        );
        assert!(p.handle(3, &words(false, &[99])).unwrap().is_empty());
    }

    #[tokio::test]
    async fn framing_exact_reads_override_and_limits() {
        assert_eq!(header(1, 0x123456).unwrap(), [1, 0x56, 0x34, 0x12]);
        assert_eq!(header(5, 0x123456).unwrap(), [5, 0x12, 0x34, 0x56]);
        assert!(header(1, MAX_BODY + 1).is_err());
        for (tag, big) in [(1, false), (5, true), (1, true), (5, false)] {
            let mut data = vec![tag];
            data.extend_from_slice(if big { &[0, 0, 2] } else { &[2, 0, 0] });
            data.extend_from_slice(&[9, 8, 7]);
            let mut input = data.as_slice();
            assert_eq!(
                read_packet(&mut input, Some(big)).await.unwrap(),
                (tag, vec![9, 8])
            );
            assert_eq!(input, &[7]);
        }
        let mut input = &[5, 0, 0, 1, 7][..];
        assert_eq!(read_packet(&mut input, None).await.unwrap(), (5, vec![7]));
        assert!(read_packet(&mut &[1, 0, 0][..], None).await.is_err());
        assert!(read_packet(&mut &[1, 2, 0, 0, 7][..], None).await.is_err());
        assert!(read_packet(&mut &[5, 0x80, 0, 1][..], None).await.is_err());
    }
}
