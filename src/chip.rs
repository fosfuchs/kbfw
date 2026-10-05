use std::ffi::{c_int, c_ulong};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::thread::sleep;
use std::time::Duration;

const IOC_READ_WRITE: u64 = 3;
const HID_TYPE: u64 = 0x48;
const SFEATURE_NR: u64 = 0x06;
const GFEATURE_NR: u64 = 0x07;

const FEAT_LEN: usize = 65;
const SECTOR: usize = 0x400;
const BLOCK: usize = 56;

const FEAT_LENS: [usize; 7] = [65, 64, 66, 33, 34, 129, 256];

unsafe extern "C" {
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
}

fn ioc(dir: u64, typ: u64, nr: u64, size: u64) -> u64 {
    (dir << 30) | (size << 16) | (typ << 8) | nr
}

fn set_feature_request(size: usize) -> u64 {
    ioc(IOC_READ_WRITE, HID_TYPE, SFEATURE_NR, size as u64)
}

fn get_feature_request(size: usize) -> u64 {
    ioc(IOC_READ_WRITE, HID_TYPE, GFEATURE_NR, size as u64)
}

fn chunk_frame(op: u8, off: u16, data: &[u8], n: usize) -> [u8; 63] {
    let mut frame = [0u8; 63];
    frame[0] = op;
    frame[1] = (off & 0xFF) as u8;
    frame[2] = (off >> 8) as u8;
    frame[3] = n as u8;
    frame[4..4 + n].copy_from_slice(&data[..n]);
    frame
}

fn find_device() -> Option<PathBuf> {
    for entry in fs::read_dir("/sys/class/hidraw").ok()?.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !name.starts_with("hidraw") {
            continue;
        }
        let Ok(uevent) = fs::read_to_string(entry.path().join("device/uevent")) else {
            continue;
        };
        if uevent.contains("048D") && uevent.contains("89DB") {
            return Some(PathBuf::from(format!("/dev/{name}")));
        }
    }
    None
}

pub struct Chip {
    pub path: PathBuf,
    feat_len: usize,
    file: File,
}

impl Chip {
    pub fn open() -> io::Result<Chip> {
        let path = find_device().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "ITE Upgrade Mode device (048d:89db) not found",
            )
        })?;
        let file = OpenOptions::new().read(true).write(true).open(&path)?;
        let mut chip = Chip {
            path,
            feat_len: FEAT_LEN,
            file,
        };
        chip.feat_len = chip.probe_feat_len()?;
        Ok(chip)
    }

    fn probe_feat_len(&mut self) -> io::Result<usize> {
        for len in FEAT_LENS {
            let mut buf = vec![0u8; len];
            let r = unsafe {
                ioctl(
                    self.file.as_raw_fd(),
                    get_feature_request(len),
                    buf.as_mut_ptr(),
                )
            };
            if r >= 0 {
                return Ok(len);
            }
        }
        Err(io::Error::other("cannot probe feature length"))
    }

    pub fn reopen(&mut self) -> bool {
        for _ in 0..20 {
            if let Some(path) = find_device()
                && let Ok(file) = OpenOptions::new().read(true).write(true).open(&path)
            {
                self.path = path;
                self.file = file;
                return true;
            }
            sleep(Duration::from_millis(500));
        }
        false
    }

    pub fn set_feature(&mut self, payload: &[u8]) -> io::Result<()> {
        let mut buf = vec![0u8; self.feat_len];
        let n = payload.len().min(self.feat_len - 1);
        buf[1..1 + n].copy_from_slice(&payload[..n]);
        let r = unsafe {
            ioctl(
                self.file.as_raw_fd(),
                set_feature_request(self.feat_len),
                buf.as_mut_ptr(),
            )
        };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn get_feature(&mut self) -> io::Result<Vec<u8>> {
        let mut buf = vec![0u8; self.feat_len];
        let r = unsafe {
            ioctl(
                self.file.as_raw_fd(),
                get_feature_request(self.feat_len),
                buf.as_mut_ptr(),
            )
        };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        buf.remove(0);
        Ok(buf)
    }

    pub fn read_block(&mut self, addr: u32, len: usize) -> io::Result<Vec<u8>> {
        let mut last = io::Error::other("read failed");
        for _ in 0..3 {
            let mut frame = [0u8; 63];
            frame[0] = 0xD1;
            frame[1] = (addr & 0xFF) as u8;
            frame[2] = ((addr >> 8) & 0xFF) as u8;
            frame[3] = ((addr >> 16) & 0xFF) as u8;
            frame[4] = len as u8;
            if let Err(e) = self.set_feature(&frame) {
                last = e;
                sleep(Duration::from_millis(50));
                continue;
            }
            sleep(Duration::from_millis(4));
            match self.get_feature() {
                Ok(r) => {
                    let echo = r.len() >= 5
                        && r[0] == 0xD1
                        && r[1] == (addr & 0xFF) as u8
                        && r[2] == ((addr >> 8) & 0xFF) as u8
                        && r[3] == ((addr >> 16) & 0xFF) as u8;
                    if echo {
                        let end = (5 + len).min(r.len());
                        return Ok(r[5..end].to_vec());
                    }
                    last = io::Error::other("echo mismatch");
                }
                Err(e) => {
                    last = e;
                    sleep(Duration::from_millis(50));
                }
            }
        }
        Err(last)
    }

    pub fn read_sector(&mut self, sector: u32) -> io::Result<Vec<u8>> {
        let mut out = Vec::with_capacity(SECTOR);
        let mut off = 0usize;
        while off < SECTOR {
            let block = self.read_block(sector + off as u32, BLOCK)?;
            out.extend_from_slice(&block);
            off += BLOCK;
        }
        out.truncate(SECTOR);
        Ok(out)
    }

    pub fn write_sector(&mut self, sector: u32, data: &[u8], pace: Duration) -> io::Result<()> {
        for i in 0..18usize {
            let off = i * 57;
            let n = if i < 17 { 57 } else { 55 };
            let frame = chunk_frame(0xC1, off as u16, &data[off..off + n], n);
            self.set_feature(&frame)?;
            sleep(pace);
        }
        let frame = chunk_frame(0xD0, 0x2AC, &data[0x2AC..0x2AC + 57], 57);
        self.set_feature(&frame)?;
        sleep(pace);
        sleep(Duration::from_millis(3));
        self.get_feature()?;
        let mut c3 = [0u8; 63];
        c3[0] = 0xC3;
        c3[1] = (sector & 0xFF) as u8;
        c3[2] = ((sector >> 8) & 0xFF) as u8;
        c3[3] = ((sector >> 16) & 0xFF) as u8;
        c3[4] = 0x00;
        c3[5] = 0x04;
        self.set_feature(&c3)?;
        sleep(pace);
        let frame = chunk_frame(0xC0, 0x72, &data[0x72..0x72 + 57], 57);
        self.set_feature(&frame)?;
        sleep(pace);
        sleep(Duration::from_millis(3));
        self.get_feature()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioc_matches_kernel_macros() {
        assert_eq!(set_feature_request(65), 0xC0414806);
        assert_eq!(get_feature_request(65), 0xC0414807);
    }

    #[test]
    fn sector_reads_nineteen_blocks() {
        assert_eq!((0..SECTOR).step_by(BLOCK).count(), 19);
    }

    #[test]
    fn chunk_frame_layout() {
        let data: Vec<u8> = (0u8..57).collect();
        let frame = chunk_frame(0xC1, 0x39, &data, 57);
        assert_eq!(frame.len(), 63);
        assert_eq!(frame[0], 0xC1);
        assert_eq!(frame[1], 0x39);
        assert_eq!(frame[2], 0x00);
        assert_eq!(frame[3], 57);
        assert_eq!(&frame[4..61], &data[..]);
    }
}
