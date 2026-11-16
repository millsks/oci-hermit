//! Control protocol between the oci-hermit host tool and the guest agent.
//!
//! The channel is a virtio-serial port named [`CONTROL_PORT`]. On the host, QEMU connects that port to its own stdin
//! and stdout, which oci-hermit holds as an anonymous pipe pair: nothing on the filesystem or the network can reach
//! it. Each message is one line of JSON, at most [`MAX_LINE`] bytes, tagged by `type`.
//!
//! A session starts with the host's [`HostMessage::Hello`]; the agent answers [`GuestMessage::Ready`] when the
//! major versions match and [`GuestMessage::Error`] when they do not. Anything that is not a known message gets an
//! [`GuestMessage::Error`] and the session goes on: neither side trusts the other to send only valid lines.

use serde::{Deserialize, Serialize};

/// Protocol version, `(major, minor)`. A different major version is refused; a newer minor adds only messages or
/// fields the other side may ignore.
pub const PROTOCOL_VERSION: (u16, u16) = (1, 0);

/// The virtio-serial port name the control channel uses.
pub const CONTROL_PORT: &str = "org.oci-hermit.control";

/// The longest line either side accepts, newline included. Longer lines are dropped whole.
pub const MAX_LINE: usize = 64 * 1024;

/// What the agent prints on the guest console once the guest is set up. The console is for people; the host waits
/// for [`GuestMessage::Ready`] on the control channel instead.
pub const CONSOLE_READY: &str = "hermit-agent: ready (control protocol";

/// Messages sent from the host tool to the guest agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostMessage {
    /// First message on a new channel.
    Hello { version: (u16, u16) },
    /// Sync filesystems and power the guest off.
    Shutdown,
}

/// Messages sent from the guest agent to the host tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GuestMessage {
    /// Reply to a compatible `Hello`.
    Ready { version: (u16, u16) },
    /// Reply to `Shutdown`, sent before the guest powers off.
    ShuttingDown,
    /// A message was refused or failed.
    Error { message: String },
}

/// Whether a peer speaking `version` can talk to this side.
pub fn compatible(version: (u16, u16)) -> bool {
    version.0 == PROTOCOL_VERSION.0
}

/// Encode a message as one line of JSON, including the trailing newline.
pub fn encode<T: Serialize>(msg: &T) -> serde_json::Result<String> {
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    Ok(line)
}

/// Decode one line of JSON into a message.
pub fn decode<'a, T: Deserialize<'a>>(line: &'a str) -> serde_json::Result<T> {
    serde_json::from_str(line.trim_end())
}

/// One unit the [`Framer`] cut out of the byte stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// A complete line, without its newline.
    Line(String),
    /// A line longer than [`MAX_LINE`]; its bytes were dropped up to the next newline.
    TooLong,
    /// A complete line that was not valid UTF-8.
    NotUtf8,
}

/// Cuts a byte stream into lines of at most [`MAX_LINE`] bytes, however the bytes arrive.
#[derive(Debug, Default)]
pub struct Framer {
    buf: Vec<u8>,
    discarding: bool,
}

impl Framer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes and collect every frame they complete.
    pub fn push(&mut self, mut data: &[u8], frames: &mut Vec<Frame>) {
        while !data.is_empty() {
            let newline = data.iter().position(|&b| b == b'\n');
            let (chunk, rest, ended) = match newline {
                Some(i) => (&data[..i], &data[i + 1..], true),
                None => (data, &[][..], false),
            };
            if !self.discarding {
                if self.buf.len() + chunk.len() + usize::from(ended) > MAX_LINE {
                    self.buf.clear();
                    self.discarding = true;
                    frames.push(Frame::TooLong);
                } else {
                    self.buf.extend_from_slice(chunk);
                }
            }
            if ended {
                if !self.discarding {
                    let line = std::mem::take(&mut self.buf);
                    frames.push(match String::from_utf8(line) {
                        Ok(text) => Frame::Line(text),
                        Err(_) => Frame::NotUtf8,
                    });
                }
                self.discarding = false;
            }
            data = rest;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(chunks: &[&[u8]]) -> Vec<Frame> {
        let mut framer = Framer::new();
        let mut out = Vec::new();
        for chunk in chunks {
            framer.push(chunk, &mut out);
        }
        out
    }

    #[test]
    fn messages_round_trip_through_the_framer() {
        let hello = encode(&HostMessage::Hello {
            version: PROTOCOL_VERSION,
        })
        .unwrap();
        let shutdown = encode(&HostMessage::Shutdown).unwrap();
        let stream = format!("{hello}{shutdown}");
        let got = frames(&[stream.as_bytes()]);
        let decoded: Vec<HostMessage> = got
            .iter()
            .map(|f| match f {
                Frame::Line(l) => decode(l).unwrap(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            decoded,
            [
                HostMessage::Hello {
                    version: PROTOCOL_VERSION
                },
                HostMessage::Shutdown
            ]
        );
    }

    #[test]
    fn messages_are_tagged_and_strict() {
        let line = encode(&GuestMessage::Error { message: "nope".into() }).unwrap();
        assert!(line.contains("\"type\":\"error\""));
        assert_eq!(
            encode(&GuestMessage::ShuttingDown).unwrap(),
            "{\"type\":\"shutting_down\"}\n"
        );
        assert!(decode::<HostMessage>(r#"{"type":"launch_missiles"}"#).is_err());
        // A newer minor version may add fields; this side ignores them.
        assert_eq!(
            decode::<HostMessage>(r#"{"type":"shutdown","added_in_1_1":true}"#).unwrap(),
            HostMessage::Shutdown
        );
        assert!(decode::<HostMessage>(r#"{"type":"hello"}"#).is_err());
        assert!(decode::<HostMessage>("").is_err());
    }

    #[test]
    fn only_the_major_version_must_match() {
        assert!(compatible(PROTOCOL_VERSION));
        assert!(compatible((PROTOCOL_VERSION.0, PROTOCOL_VERSION.1 + 7)));
        assert!(!compatible((PROTOCOL_VERSION.0 + 1, 0)));
    }

    #[test]
    fn lines_split_across_chunks_are_joined() {
        assert_eq!(
            frames(&[b"ab", b"c\nd", b"", b"e\n"]),
            [Frame::Line("abc".into()), Frame::Line("de".into())]
        );
        assert_eq!(frames(&[b"no newline yet"]), []);
        assert_eq!(frames(&[b"\n"]), [Frame::Line(String::new())]);
    }

    #[test]
    fn an_overlong_line_is_dropped_once_and_the_next_line_is_kept() {
        let long = vec![b'x'; MAX_LINE * 2];
        let got = frames(&[&long, &long, b"tail\nnext\n"]);
        assert_eq!(got, [Frame::TooLong, Frame::Line("next".into())]);
        let exact = vec![b'y'; MAX_LINE - 1];
        let got = frames(&[&exact, b"\n"]);
        assert_eq!(got, [Frame::Line("y".repeat(MAX_LINE - 1))]);
        let one_over = vec![b'z'; MAX_LINE];
        assert_eq!(frames(&[&one_over, b"\n"]), [Frame::TooLong]);
    }

    #[test]
    fn invalid_utf8_is_reported_not_passed_on() {
        assert_eq!(
            frames(&[b"\xff\xfe\n", b"ok\n"]),
            [Frame::NotUtf8, Frame::Line("ok".into())]
        );
    }

    /// A small, seeded xorshift generator, so the fuzz test is reproducible without a dependency.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// Bytes that are sometimes valid messages, sometimes mutated ones and sometimes noise, with newlines.
    fn input(rng: &mut Rng) -> Vec<u8> {
        let valid = [
            encode(&HostMessage::Hello {
                version: PROTOCOL_VERSION,
            })
            .unwrap(),
            encode(&HostMessage::Shutdown).unwrap(),
            encode(&GuestMessage::Error { message: "x".into() }).unwrap(),
        ];
        let mut out = Vec::new();
        for _ in 0..rng.below(8) {
            match rng.below(4) {
                0 => out.extend_from_slice(valid[rng.below(valid.len())].as_bytes()),
                1 => {
                    let mut msg = valid[rng.below(valid.len())].clone().into_bytes();
                    for _ in 0..=rng.below(4) {
                        let at = rng.below(msg.len());
                        msg[at] = rng.next() as u8;
                    }
                    out.extend_from_slice(&msg);
                }
                2 => out.extend((0..rng.below(64)).map(|_| rng.next() as u8)),
                _ => out.resize(out.len() + rng.below(MAX_LINE * 2), b'{'),
            }
            if rng.below(2) == 0 {
                out.push(b'\n');
            }
        }
        out
    }

    #[test]
    fn fuzz_the_framer_and_decoders() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for _ in 0..2_000 {
            let data = input(&mut rng);
            let mut framer = Framer::new();
            let mut got = Vec::new();
            let mut rest = &data[..];
            while !rest.is_empty() {
                let take = 1 + rng.below(rest.len().min(4096));
                framer.push(&rest[..take], &mut got);
                rest = &rest[take..];
            }
            let lines = data.iter().filter(|&&b| b == b'\n').count();
            assert!(got.len() <= lines + data.len() / MAX_LINE + 1);
            for frame in got {
                if let Frame::Line(line) = frame {
                    assert!(line.len() < MAX_LINE, "a line over the cap got through");
                    assert!(!line.contains('\n'));
                    let _ = decode::<HostMessage>(&line);
                    let _ = decode::<GuestMessage>(&line);
                }
            }
        }
    }
}
