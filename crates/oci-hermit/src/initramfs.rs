//! The guest's initramfs: the agent as `/init`, plus the console device it logs to.
//!
//! Written as a `newc` cpio archive in memory. Device nodes are records in the archive, so building it needs no
//! privileges on the host.

/// `newc` mode bits for the entries written here.
const DIR: u32 = 0o040_755;
const CHAR_DEVICE: u32 = 0o020_600;
const EXECUTABLE: u32 = 0o100_755;

/// `/dev/console` is character device 5:1 on Linux.
const CONSOLE: (u32, u32) = (5, 1);

fn pad4(buf: &mut Vec<u8>) {
    while buf.len() % 4 != 0 {
        buf.push(0);
    }
}

fn record(buf: &mut Vec<u8>, ino: u32, name: &str, mode: u32, rdev: (u32, u32), data: &[u8]) {
    let name_size = u32::try_from(name.len() + 1).expect("entry name fits in u32");
    let file_size = u32::try_from(data.len()).expect("entry data fits in u32");
    // magic, then ino, mode, uid, gid, nlink, mtime, filesize, devmajor, devminor, rdevmajor, rdevminor, namesize,
    // check: thirteen 8-digit hex fields.
    let fields = [ino, mode, 0, 0, 1, 0, file_size, 0, 0, rdev.0, rdev.1, name_size, 0];
    buf.extend_from_slice(b"070701");
    for field in fields {
        buf.extend_from_slice(format!("{field:08x}").as_bytes());
    }
    buf.extend_from_slice(name.as_bytes());
    buf.push(0);
    pad4(buf);
    buf.extend_from_slice(data);
    pad4(buf);
}

/// An initramfs that boots `agent` as PID 1, with `/dev/console` so its output reaches the serial console.
pub fn build(agent: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(agent.len() + 512);
    record(&mut buf, 1, "dev", DIR, (0, 0), &[]);
    record(&mut buf, 2, "dev/console", CHAR_DEVICE, CONSOLE, &[]);
    record(&mut buf, 3, "init", EXECUTABLE, (0, 0), agent);
    record(&mut buf, 0, "TRAILER!!!", 0, (0, 0), &[]);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One entry read back from an archive.
    #[derive(Debug, PartialEq)]
    struct Entry {
        name: String,
        mode: u32,
        rdev: (u32, u32),
        data: Vec<u8>,
    }

    fn hex(bytes: &[u8]) -> u32 {
        u32::from_str_radix(std::str::from_utf8(bytes).unwrap(), 16).unwrap()
    }

    fn parse(mut archive: &[u8]) -> Vec<Entry> {
        let mut entries = Vec::new();
        let mut offset = 0;
        loop {
            assert_eq!(&archive[..6], b"070701", "magic at offset {offset}");
            let field = |i: usize| hex(&archive[6 + 8 * i..6 + 8 * (i + 1)]);
            let (mode, size, rdev, name_size) =
                (field(1), field(6) as usize, (field(9), field(10)), field(11) as usize);
            let header = 6 + 13 * 8;
            let name = String::from_utf8(archive[header..header + name_size - 1].to_vec()).unwrap();
            let data_start = (header + name_size).next_multiple_of(4);
            let data = archive[data_start..data_start + size].to_vec();
            let next = (data_start + size).next_multiple_of(4);
            offset += next;
            archive = &archive[next..];
            if name == "TRAILER!!!" {
                assert!(archive.is_empty(), "nothing after the trailer");
                return entries;
            }
            entries.push(Entry { name, mode, rdev, data });
        }
    }

    #[test]
    fn holds_the_agent_as_init_and_a_console_node() {
        let archive = build(b"\x7fELF agent");
        assert_eq!(archive.len() % 4, 0);
        let entries = parse(&archive);
        assert_eq!(
            entries,
            [
                Entry {
                    name: "dev".into(),
                    mode: DIR,
                    rdev: (0, 0),
                    data: vec![]
                },
                Entry {
                    name: "dev/console".into(),
                    mode: CHAR_DEVICE,
                    rdev: (5, 1),
                    data: vec![]
                },
                Entry {
                    name: "init".into(),
                    mode: EXECUTABLE,
                    rdev: (0, 0),
                    data: b"\x7fELF agent".to_vec()
                },
            ]
        );
    }

    #[test]
    fn pads_every_data_length() {
        for len in 0..9 {
            let agent = vec![0xab; len];
            let entries = parse(&build(&agent));
            assert_eq!(entries[2].data, agent);
        }
    }
}
