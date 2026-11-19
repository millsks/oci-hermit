//! Control protocol between the oci-hermit host tool and the guest agent.
//!
//! The channel is a virtio-serial port named [`CONTROL_PORT`]. On the host, QEMU connects that port to its own stdin
//! and stdout, which oci-hermit holds as an anonymous pipe pair: nothing on the filesystem or the network can reach
//! it. Each message is one line of JSON, at most [`MAX_LINE`] bytes, tagged by `type`.
//!
//! A session starts with the host's [`HostMessage::Hello`]; the agent answers [`GuestMessage::Ready`] when the
//! major versions match and [`GuestMessage::Error`] when they do not. Anything that is not a known message gets an
//! [`GuestMessage::Error`] and the session goes on: neither side trusts the other to send only valid lines.
//!
//! Since 1.1 the host can [`HostMessage::Run`] one command: the agent answers [`GuestMessage::Started`], streams
//! its output as [`GuestMessage::Output`], takes its input as [`HostMessage::Stdin`], and reports
//! [`GuestMessage::Exited`]. Bytes travel base64-encoded ([`b64`]), at most [`CHUNK`] raw bytes a message.
//!
//! Since 1.2 `Run` can name the [`User`] to run the command as. An older agent would ignore that field and run the
//! command as root, so the host sends it only to an agent that speaks 1.2 or later.
//!
//! Since 1.3 `Run` can name the image's `layers`: the serial numbers of the read-only virtio disks that hold them,
//! bottom layer first. The agent stacks them with overlayfs under a throwaway writable layer and runs the command
//! there; without them the image is in the initramfs, as before. An older agent would ignore the field and find no
//! image, so the host sends it only to an agent that speaks 1.3 or later.
//!
//! Since 1.4 the agent runs the command in a container, with crun: its own PID, mount, IPC, UTS, network and cgroup
//! namespaces and Docker's default capabilities, rather than chrooted as root's peer. `Run` can name the container's
//! `hostname`. An older agent would run the command outside a container, so the host refuses one.
//!
//! Since 1.5 the host can [`HostMessage::Exec`] more commands in the running container (`oci-hermit exec`). Each
//! carries a `process` number the host picks, from 1; the messages about a command's input, output and end carry it
//! too. The command `Run` started is process 0, and the field is left out for it, so 1.4 peers see the messages they
//! know.
//!
//! Since 1.6 the host can [`HostMessage::Signal`] the container's command (`stop`, `kill`, Ctrl-C), which the agent
//! passes on with `crun kill`. Signals are Linux's numbers ([`signal_number`]), the same on every guest architecture.

use serde::{Deserialize, Serialize};

/// Protocol version, `(major, minor)`. A different major version is refused; a newer minor adds only messages or
/// fields the other side may ignore.
pub const PROTOCOL_VERSION: (u16, u16) = (1, 6);

/// The virtio-serial port name the control channel uses.
pub const CONTROL_PORT: &str = "org.oci-hermit.control";

/// The longest line either side accepts, newline included. Longer lines are dropped whole.
pub const MAX_LINE: usize = 64 * 1024;

/// The most raw bytes one `Stdin` or `Output` message carries; base64 makes that about 22 KB, well under
/// [`MAX_LINE`].
pub const CHUNK: usize = 16 * 1024;

/// What the agent prints on the guest console once the guest is set up. The console is for people; the host waits
/// for [`GuestMessage::Ready`] on the control channel instead.
pub const CONSOLE_READY: &str = "hermit-agent: ready (control protocol";

/// Which command a message is about: 0 for the one `Run` started, the number the host gave an `Exec` otherwise (1.5).
pub type Process = u32;

/// The command `Run` started.
pub const MAIN: Process = 0;

#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes a reference"
)]
fn is_main(process: &Process) -> bool {
    *process == MAIN
}

/// Messages sent from the host tool to the guest agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostMessage {
    /// First message on a new channel.
    Hello { version: (u16, u16) },
    /// Sync filesystems and power the guest off.
    Shutdown,
    /// Start a command in the image's root filesystem (1.1). One command per session.
    Run {
        /// The command and its arguments; the first is looked up in `PATH` from `env` when it has no `/`.
        argv: Vec<String>,
        /// `NAME=value` pairs.
        env: Vec<String>,
        /// Working directory inside the image.
        cwd: String,
        /// Give the command a terminal (`-t`); otherwise its stdin, stdout and stderr are pipes.
        tty: bool,
        /// The terminal's size, when there is one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rows: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cols: Option<u16>,
        /// Who to run the command as (1.2); root when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user: Option<User>,
        /// The serial numbers of the disks that hold the image's layers, bottom first (1.3); none when the image
        /// is in the initramfs.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        layers: Vec<String>,
        /// The container's hostname (1.4); the agent picks one when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hostname: Option<String>,
    },
    /// Bytes for a command's stdin, base64-encoded (1.1).
    Stdin {
        #[serde(default, skip_serializing_if = "is_main")]
        process: Process,
        data: String,
    },
    /// The host's stdin for a command reached end of file (1.1).
    StdinClosed {
        #[serde(default, skip_serializing_if = "is_main")]
        process: Process,
    },
    /// Send `signal` to a command (1.6); only the container's own, process 0, takes signals so far. There is no
    /// reply: what the signal does shows in the command's output and exit.
    Signal {
        #[serde(default, skip_serializing_if = "is_main")]
        process: Process,
        signal: i32,
    },
    /// Start another command in the container `Run` started (1.5), as `process`, a number not in use.
    Exec {
        process: Process,
        argv: Vec<String>,
        env: Vec<String>,
        cwd: String,
        tty: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rows: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cols: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user: Option<User>,
    },
}

/// The identity a command runs as, already resolved against the image's `/etc/passwd` and `/etc/group`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub uid: u32,
    pub gid: u32,
    /// Supplementary group IDs.
    #[serde(default)]
    pub groups: Vec<u32>,
}

/// Linux's signal names, as `docker kill -s` and an image's `StopSignal` give them, by number.
const SIGNALS: [&str; 31] = [
    "HUP", "INT", "QUIT", "ILL", "TRAP", "ABRT", "BUS", "FPE", "KILL", "USR1", "SEGV", "USR2", "PIPE", "ALRM", "TERM",
    "STKFLT", "CHLD", "CONT", "STOP", "TSTP", "TTIN", "TTOU", "URG", "XCPU", "XFSZ", "VTALRM", "PROF", "WINCH", "IO",
    "PWR", "SYS",
];

/// The highest signal number Linux has (`SIGRTMAX`).
pub const MAX_SIGNAL: i32 = 64;

/// The Linux signal `name` names: `TERM`, `SIGTERM`, `sigterm` or `15`; real-time ones as `RTMIN+3`.
pub fn signal_number(name: &str) -> Option<i32> {
    let upper = name.trim().to_ascii_uppercase();
    let bare = upper.strip_prefix("SIG").unwrap_or(&upper);
    // `RTMIN`, `RTMIN+n`, `RTMAX`, `RTMAX-n`.
    let offset = |rest: &str, sign: char| -> Option<i32> {
        if rest.is_empty() {
            Some(0)
        } else {
            rest.strip_prefix(sign)?.parse().ok()
        }
    };
    let number = if let Ok(n) = bare.parse::<i32>() {
        n
    } else if let Some(i) = SIGNALS.iter().position(|s| *s == bare) {
        i32::try_from(i).ok()? + 1
    } else if let Some(rest) = bare.strip_prefix("RTMIN") {
        34 + offset(rest, '+')?
    } else {
        MAX_SIGNAL - offset(bare.strip_prefix("RTMAX")?, '-')?
    };
    (1..=MAX_SIGNAL).contains(&number).then_some(number)
}

/// Messages sent from the guest agent to the host tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GuestMessage {
    /// Reply to a compatible `Hello`.
    Ready { version: (u16, u16) },
    /// Reply to `Shutdown`, sent before the guest powers off.
    ShuttingDown,
    /// A message was refused or failed; an `Exec` that could not start names its process.
    Error {
        #[serde(default, skip_serializing_if = "is_main")]
        process: Process,
        message: String,
    },
    /// The command from `Run` or `Exec` started (1.1).
    Started {
        #[serde(default, skip_serializing_if = "is_main")]
        process: Process,
        pid: i32,
    },
    /// Output from a command, base64-encoded (1.1). With a terminal everything is `stdout`.
    Output {
        #[serde(default, skip_serializing_if = "is_main")]
        process: Process,
        stream: Stream,
        data: String,
    },
    /// A command ended (1.1): its exit status, or 128 plus the signal that killed it.
    Exited {
        #[serde(default, skip_serializing_if = "is_main")]
        process: Process,
        code: i32,
    },
}

/// Which output stream an [`GuestMessage::Output`] belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stream {
    Stdout,
    Stderr,
}

/// Standard base64 (RFC 4648, with padding), for the bytes in `Stdin` and `Output`.
pub mod b64 {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let n = chunk
                .iter()
                .enumerate()
                .fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(char::from(ALPHABET[(n >> (18 - 6 * i) & 63) as usize]));
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    /// The bytes `text` encodes, or `None` if it is not valid padded base64.
    pub fn decode(text: &str) -> Option<Vec<u8>> {
        let text = text.as_bytes();
        if !text.len().is_multiple_of(4) {
            return None;
        }
        let value = |c: u8| ALPHABET.iter().position(|&a| a == c).map(|v| v as u32);
        let mut out = Vec::with_capacity(text.len() / 4 * 3);
        for (index, quad) in text.chunks(4).enumerate() {
            let last = index == text.len() / 4 - 1;
            let pad = quad.iter().rev().take_while(|&&c| c == b'=').count();
            if pad > 2 || (pad > 0 && !last) {
                return None;
            }
            let mut n = 0u32;
            for (i, &c) in quad[..4 - pad].iter().enumerate() {
                n |= value(c)? << (18 - 6 * i);
            }
            // The bits under the padding must be zero, so every value has exactly one encoding.
            if n & ((1u32 << (8 * pad)) - 1) != 0 {
                return None;
            }
            let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
            out.extend_from_slice(&bytes[..3 - pad]);
        }
        Some(out)
    }
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
        let line = encode(&GuestMessage::Error {
            process: MAIN,
            message: "nope".into(),
        })
        .unwrap();
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
    fn base64_matches_rfc_4648() {
        let cases: [(&[u8], &str); 7] = [
            (b"", ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"fooba", "Zm9vYmE="),
            (b"foobar", "Zm9vYmFy"),
        ];
        for (raw, text) in cases {
            assert_eq!(b64::encode(raw), text);
            assert_eq!(b64::decode(text).unwrap(), raw);
        }
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(b64::decode(&b64::encode(&all)).unwrap(), all);
        for bad in ["Zg=", "Z===", "Zg==Zg==", "Z!==", "Zm9v\n", "Zh==", "Zm9=", "Zm+="] {
            assert_eq!(b64::decode(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn run_messages_round_trip() {
        let run = HostMessage::Run {
            argv: vec!["sh".into(), "-c".into(), "exit 7".into()],
            env: vec!["PATH=/bin".into()],
            cwd: "/".into(),
            tty: true,
            rows: Some(24),
            cols: None,
            user: Some(User {
                uid: 1000,
                gid: 1000,
                groups: vec![27],
            }),
            layers: vec!["layer0".into(), "layer1".into()],
            hostname: Some("4f2a9c1d0b3e".into()),
        };
        assert_eq!(decode::<HostMessage>(&encode(&run).unwrap()).unwrap(), run);
        // A 1.1 host sends no user, a 1.2 host no layers and a 1.3 host no hostname, and that still decodes.
        let old = r#"{"type":"run","argv":["id"],"env":[],"cwd":"/","tty":false}"#;
        assert!(matches!(
            decode::<HostMessage>(old).unwrap(),
            HostMessage::Run { user: None, ref layers, hostname: None, .. } if layers.is_empty()
        ));
        let output = GuestMessage::Output {
            process: MAIN,
            stream: Stream::Stderr,
            data: b64::encode(&[0u8; CHUNK]),
        };
        let line = encode(&output).unwrap();
        assert!(line.len() < MAX_LINE, "a full chunk fits in one line");
        assert!(line.contains(r#""stream":"stderr""#));
        assert_eq!(decode::<GuestMessage>(&line).unwrap(), output);
    }

    #[test]
    fn exec_messages_name_their_process_and_the_main_command_stays_as_it_was() {
        let exec = HostMessage::Exec {
            process: 2,
            argv: vec!["sh".into()],
            env: vec!["PATH=/bin".into()],
            cwd: "/srv".into(),
            tty: true,
            rows: Some(24),
            cols: Some(80),
            user: None,
        };
        assert_eq!(decode::<HostMessage>(&encode(&exec).unwrap()).unwrap(), exec);
        // The command `Run` started is process 0, and its messages are what a 1.4 peer sends and expects.
        let cases: [(GuestMessage, &str); 3] = [
            (
                GuestMessage::Exited { process: MAIN, code: 3 },
                r#"{"type":"exited","code":3}"#,
            ),
            (
                GuestMessage::Started { process: MAIN, pid: 7 },
                r#"{"type":"started","pid":7}"#,
            ),
            (
                GuestMessage::Error {
                    process: MAIN,
                    message: "no".into(),
                },
                r#"{"type":"error","message":"no"}"#,
            ),
        ];
        for (message, line) in cases {
            assert_eq!(encode(&message).unwrap(), format!("{line}\n"));
            assert_eq!(decode::<GuestMessage>(line).unwrap(), message);
        }
        assert_eq!(
            encode(&HostMessage::StdinClosed { process: MAIN }).unwrap(),
            "{\"type\":\"stdin_closed\"}\n"
        );
        let to_exec = HostMessage::Stdin {
            process: 2,
            data: "aGkK".into(),
        };
        let line = encode(&to_exec).unwrap();
        assert!(line.contains(r#""process":2"#), "{line}");
        assert_eq!(decode::<HostMessage>(&line).unwrap(), to_exec);
        let from_exec = GuestMessage::Exited { process: 2, code: 0 };
        assert_eq!(decode::<GuestMessage>(&encode(&from_exec).unwrap()).unwrap(), from_exec);
    }

    #[test]
    fn signals_go_by_linux_numbers() {
        for (name, number) in [
            ("TERM", 15),
            ("SIGTERM", 15),
            ("sigint", 2),
            ("KILL", 9),
            ("HUP", 1),
            ("SYS", 31),
            ("9", 9),
            ("SIGWINCH", 28),
            ("RTMIN", 34),
            ("RTMIN+3", 37),
            ("RTMAX", 64),
            ("RTMAX-1", 63),
        ] {
            assert_eq!(signal_number(name), Some(number), "{name}");
        }
        for bad in ["", "0", "65", "-1", "NOPE", "RTMIN+31", "RTMIN3", "SIG"] {
            assert_eq!(signal_number(bad), None, "{bad}");
        }
        let signal = HostMessage::Signal {
            process: MAIN,
            signal: 15,
        };
        let line = encode(&signal).unwrap();
        assert_eq!(line, "{\"type\":\"signal\",\"signal\":15}\n");
        assert_eq!(decode::<HostMessage>(&line).unwrap(), signal);
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
            encode(&GuestMessage::Error {
                process: MAIN,
                message: "x".into(),
            })
            .unwrap(),
            encode(&HostMessage::Stdin {
                process: 3,
                data: b64::encode(b"ls -l\r"),
            })
            .unwrap(),
            encode(&GuestMessage::Exited { process: MAIN, code: 7 }).unwrap(),
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
                    if let Some(bytes) = b64::decode(&line) {
                        assert_eq!(b64::encode(&bytes), line, "decode accepts only canonical base64");
                    }
                }
            }
        }
    }
}
