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
const BLOCK: usize = 56;

pub const SECTOR: usize = 0x400;
const APP_END: u32 = 0xE000;

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

fn erase_frame(addr: u32, count: u16) -> [u8; 63] {
    let mut frame = [0u8; 63];
    frame[0] = 0xC2;
    frame[1] = (addr & 0xFF) as u8;
    frame[2] = ((addr >> 8) & 0xFF) as u8;
    frame[3] = ((addr >> 16) & 0xFF) as u8;
    frame[4] = (count & 0xFF) as u8;
    frame[5] = (count >> 8) as u8;
    frame
}

fn sum16(data: &[u8]) -> u16 {
    data.iter().fold(0u16, |a, &b| a.wrapping_add(b as u16))
}

const FLAT_BOOT_START: u32 = 0x3F800;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Topology {
    Mirrored,
    Flat,
}

fn erase_allowed(sector: u32, topo: Topology) -> bool {
    if !sector.is_multiple_of(SECTOR as u32) {
        return false;
    }
    match topo {
        Topology::Mirrored => (sector & 0xFFFF) + SECTOR as u32 <= APP_END,
        Topology::Flat => sector + SECTOR as u32 <= FLAT_BOOT_START,
    }
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

pub trait Transport {
    fn set(&mut self, payload: &[u8]) -> io::Result<()>;
    fn get(&mut self) -> io::Result<Vec<u8>>;
    fn reopen(&mut self) -> bool;
}

pub struct Hidraw {
    pub path: PathBuf,
    feat_len: usize,
    file: File,
}

impl Hidraw {
    fn open_default() -> io::Result<Hidraw> {
        let path = find_device().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "ITE Upgrade Mode device (048d:89db) not found",
            )
        })?;
        let file = OpenOptions::new().read(true).write(true).open(&path)?;
        let mut dev = Hidraw {
            path,
            feat_len: FEAT_LEN,
            file,
        };
        dev.feat_len = dev.probe_feat_len()?;
        Ok(dev)
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
}

impl Transport for Hidraw {
    fn set(&mut self, payload: &[u8]) -> io::Result<()> {
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

    fn get(&mut self) -> io::Result<Vec<u8>> {
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

    fn reopen(&mut self) -> bool {
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
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SumCheck {
    Unknown,
    Off,
    On,
}

pub struct Chip {
    pub path: PathBuf,
    dev: Box<dyn Transport>,
    erase: bool,
    sum: SumCheck,
    topo: Option<Topology>,
    erase_polls: u32,
    block_delay: Duration,
}

impl Chip {
    pub fn open() -> io::Result<Chip> {
        let dev = Hidraw::open_default()?;
        Ok(Chip {
            path: dev.path.clone(),
            dev: Box::new(dev),
            erase: true,
            sum: SumCheck::Unknown,
            topo: None,
            erase_polls: 40,
            block_delay: Duration::from_millis(4),
        })
    }

    pub fn set_erase(&mut self, on: bool) {
        self.erase = on;
    }

    pub fn erase_enabled(&self) -> bool {
        self.erase
    }

    pub fn reopen(&mut self) -> bool {
        self.dev.reopen()
    }

    pub fn set_feature(&mut self, payload: &[u8]) -> io::Result<()> {
        self.dev.set(payload)
    }

    pub fn get_feature(&mut self) -> io::Result<Vec<u8>> {
        self.dev.get()
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
            sleep(self.block_delay);
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

    pub fn topology(&mut self) -> io::Result<Topology> {
        if let Some(topo) = self.topo {
            return Ok(topo);
        }
        let a = self.read_block(0, BLOCK)?;
        let b = self.read_block(0x10000, BLOCK)?;
        let c = self.read_block(0x30000, BLOCK)?;
        let topo = if a == b && b == c {
            Topology::Mirrored
        } else {
            Topology::Flat
        };
        self.topo = Some(topo);
        Ok(topo)
    }

    pub fn erase_sector(&mut self, sector: u32) -> io::Result<()> {
        let topo = self.topology()?;
        if !erase_allowed(sector, topo) {
            return Err(io::Error::other(format!(
                "refusing to erase {sector:#x}: {topo:?} layout, erase would touch the bootloader or is not 1 KiB aligned"
            )));
        }
        self.set_feature(&erase_frame(sector, 1))?;
        sleep(Duration::from_millis(30));
        let mut last = String::from("no read-back");
        for _ in 0..self.erase_polls {
            match self.read_sector(sector) {
                Ok(r) => match r.iter().position(|&b| b != 0xFF) {
                    None => return Ok(()),
                    Some(i) => last = format!("not blank, first non-FF byte at +{i:#x}"),
                },
                Err(e) => last = format!("read error: {e}"),
            }
            sleep(Duration::from_millis(40));
        }
        Err(io::Error::other(format!(
            "erase of {sector:#x} not confirmed ({last})"
        )))
    }

    fn stage(&mut self, data: &[u8], pace: Duration) -> io::Result<Option<u16>> {
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
        let r = self.get_feature()?;
        Ok(if r.len() >= 3 {
            Some((r[1] as u16) | ((r[2] as u16) << 8))
        } else {
            None
        })
    }

    fn program(&mut self, sector: u32, data: &[u8], pace: Duration) -> io::Result<()> {
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

    pub fn write_sector(&mut self, sector: u32, data: &[u8], pace: Duration) -> io::Result<()> {
        if data.len() < SECTOR {
            return Err(io::Error::other(format!(
                "sector data must be {SECTOR} bytes, got {}",
                data.len()
            )));
        }
        let data = &data[..SECTOR];
        if self.erase
            && let Err(e) = self.erase_sector(sector)
        {
            eprintln!("kbfw: erase of {sector:#x} not confirmed ({e}), programming anyway");
        }
        let chip_sum = self.stage(data, pace)?;
        if let (SumCheck::On, Some(got)) = (self.sum, chip_sum)
            && sum16(data) != got
        {
            eprintln!(
                "kbfw: sector {sector:#x}: staging checksum mismatch, host {:#06x} chip {got:#06x}, not programmed",
                sum16(data)
            );
            return Err(io::Error::other("staging checksum mismatch"));
        }
        if let (SumCheck::Unknown, Some(got)) = (self.sum, chip_sum) {
            if sum16(data) == got {
                self.sum = SumCheck::On;
            } else {
                self.sum = SumCheck::Off;
                eprintln!(
                    "kbfw: chip does not report the staging checksum (expected {:#06x}, got {got:#06x}), checksum check disabled for this session",
                    sum16(data)
                );
            }
        }
        self.program(sector, data, pace)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Clone)]
    struct SimNor {
        mem: Vec<u8>,
        staging: Vec<u8>,
        reply: Vec<u8>,
        erase_works: bool,
        program_erases: bool,
        checksum: bool,
        mirrored: bool,
        sum_bias: u16,
        erases: Vec<u32>,
        programs: Vec<u32>,
    }

    impl SimNor {
        fn dirty() -> SimNor {
            let mut state = 0x1234_5678u32;
            let mem = (0..0x40000)
                .map(|_| {
                    state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (state >> 24) as u8
                })
                .collect();
            SimNor {
                mem,
                staging: vec![0u8; SECTOR],
                reply: Vec::new(),
                erase_works: true,
                program_erases: false,
                checksum: true,
                mirrored: true,
                sum_bias: 0,
                erases: Vec::new(),
                programs: Vec::new(),
            }
        }

        fn at(&self, addr: u32) -> usize {
            if self.mirrored {
                (addr as usize) & 0xFFFF
            } else {
                (addr as usize) & 0x3FFFF
            }
        }

        fn peek(&self, addr: u32, len: usize) -> Vec<u8> {
            (0..len as u32)
                .map(|i| self.mem[self.at(addr + i)])
                .collect()
        }
    }

    struct Shared(Rc<RefCell<SimNor>>);

    impl Transport for Shared {
        fn set(&mut self, payload: &[u8]) -> io::Result<()> {
            self.0.borrow_mut().set(payload)
        }

        fn get(&mut self) -> io::Result<Vec<u8>> {
            self.0.borrow_mut().get()
        }

        fn reopen(&mut self) -> bool {
            true
        }
    }

    impl Transport for SimNor {
        fn set(&mut self, payload: &[u8]) -> io::Result<()> {
            match payload[0] {
                0xC1 | 0xD0 => {
                    let off = u16::from_le_bytes([payload[1], payload[2]]) as usize;
                    let n = payload[3] as usize;
                    self.staging[off..off + n].copy_from_slice(&payload[4..4 + n]);
                    if payload[0] == 0xD0 {
                        let sum = sum16(&self.staging).wrapping_add(self.sum_bias);
                        let b = if self.checksum {
                            sum.to_le_bytes()
                        } else {
                            [0x5A, 0xA5]
                        };
                        self.reply = vec![0xD0, b[0], b[1]];
                    }
                }
                0xC0 => {}
                0xC2 => {
                    let addr = u32::from_le_bytes([payload[1], payload[2], payload[3], 0]);
                    let count = u16::from_le_bytes([payload[4], payload[5]]) as usize;
                    self.erases.push(addr);
                    if self.erase_works {
                        for i in 0..count * SECTOR {
                            let at = self.at(addr + i as u32);
                            self.mem[at] = 0xFF;
                        }
                    }
                }
                0xC3 => {
                    let addr = u32::from_le_bytes([payload[1], payload[2], payload[3], 0]);
                    let count = u16::from_le_bytes([payload[4], payload[5]]) as usize;
                    self.programs.push(addr);
                    for i in 0..count {
                        let at = self.at(addr + i as u32);
                        if self.program_erases {
                            self.mem[at] = self.staging[i];
                        } else {
                            self.mem[at] &= self.staging[i];
                        }
                    }
                }
                0xD1 => {
                    let addr = u32::from_le_bytes([payload[1], payload[2], payload[3], 0]);
                    let len = payload[4] as usize;
                    let mut r = vec![0xD1, payload[1], payload[2], payload[3], payload[4]];
                    r.extend_from_slice(&self.peek(addr, len));
                    self.reply = r;
                }
                other => return Err(io::Error::other(format!("bad opcode {other:#x}"))),
            }
            Ok(())
        }

        fn get(&mut self) -> io::Result<Vec<u8>> {
            Ok(std::mem::take(&mut self.reply))
        }

        fn reopen(&mut self) -> bool {
            true
        }
    }

    fn sim_chip(sim: &Rc<RefCell<SimNor>>) -> Chip {
        Chip {
            path: PathBuf::from("sim"),
            dev: Box::new(Shared(sim.clone())),
            erase: true,
            sum: SumCheck::Unknown,
            topo: None,
            erase_polls: 3,
            block_delay: Duration::ZERO,
        }
    }

    fn pattern(seed: u8) -> Vec<u8> {
        (0..SECTOR)
            .map(|i| (i.wrapping_mul(37).wrapping_add(seed as usize)) as u8)
            .collect()
    }

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

    #[test]
    fn erase_frame_layout() {
        let f = erase_frame(0x9400, 1);
        assert_eq!(&f[..6], &[0xC2, 0x00, 0x94, 0x00, 0x01, 0x00]);
        assert!(f[6..].iter().all(|&b| b == 0));
        let g = erase_frame(0, 56);
        assert_eq!(&g[..6], &[0xC2, 0, 0, 0, 56, 0]);
    }

    #[test]
    fn sum16_wraps_like_vendor() {
        assert_eq!(sum16(&[0xFF; 1024]), 0xFC00);
        assert_eq!(sum16(&[1, 2, 3]), 6);
    }

    #[test]
    fn write_repairs_dirty_sector_and_spares_neighbours() {
        let sim = Rc::new(RefCell::new(SimNor::dirty()));
        let neighbours: Vec<Vec<u8>> = [0x3C00u32, 0x4400]
            .iter()
            .map(|a| sim.borrow().peek(*a, SECTOR))
            .collect();
        let mut chip = sim_chip(&sim);
        let target = pattern(11);
        chip.write_sector(0x4000, &target, Duration::ZERO).unwrap();
        assert_eq!(chip.read_sector(0x4000).unwrap(), target);
        assert_eq!(sim.borrow().peek(0x3C00, SECTOR), neighbours[0]);
        assert_eq!(sim.borrow().peek(0x4400, SECTOR), neighbours[1]);
    }

    #[test]
    fn write_is_mirrored_across_banks_and_spares_bootloader() {
        let sim = Rc::new(RefCell::new(SimNor::dirty()));
        let boot = sim.borrow().peek(0xE000, 0x200);
        let mut chip = sim_chip(&sim);
        let target = pattern(29);
        chip.write_sector(0x4000, &target, Duration::ZERO).unwrap();
        for bank in [0x14000u32, 0x24000, 0x34000] {
            assert_eq!(chip.read_sector(bank).unwrap(), target);
        }
        assert_eq!(sim.borrow().peek(0xE000, 0x200), boot);
    }

    #[test]
    fn write_without_erase_only_clears_bits() {
        let sim = Rc::new(RefCell::new(SimNor::dirty()));
        sim.borrow_mut().erase_works = false;
        let old = sim.borrow().peek(0x4000, SECTOR);
        let mut chip = sim_chip(&sim);
        chip.set_erase(false);
        let target = pattern(7);
        chip.write_sector(0x4000, &target, Duration::ZERO).unwrap();
        let got = chip.read_sector(0x4000).unwrap();
        let and: Vec<u8> = old.iter().zip(&target).map(|(a, b)| a & b).collect();
        assert_ne!(got, target);
        assert_eq!(got, and);
        assert!(sim.borrow().erases.is_empty());
    }

    #[test]
    fn write_survives_chip_without_erase_support() {
        let sim = Rc::new(RefCell::new(SimNor::dirty()));
        sim.borrow_mut().erase_works = false;
        sim.borrow_mut().program_erases = true;
        let mut chip = sim_chip(&sim);
        let target = pattern(3);
        chip.write_sector(0x4000, &target, Duration::ZERO).unwrap();
        assert_eq!(chip.read_sector(0x4000).unwrap(), target);
    }

    #[test]
    fn erase_refuses_bootloader_and_unaligned() {
        let sim = Rc::new(RefCell::new(SimNor::dirty()));
        let mut chip = sim_chip(&sim);
        for bad in [0xE000u32, 0xE400, 0xFC00, 0x402] {
            assert!(chip.erase_sector(bad).is_err(), "erase of {bad:#x} allowed");
        }
        assert!(sim.borrow().erases.is_empty());
        assert!(chip.erase_sector(0xDC00).is_ok());
    }

    #[test]
    fn checksum_mismatch_blocks_programming_once_trusted() {
        let sim = Rc::new(RefCell::new(SimNor::dirty()));
        let mut chip = sim_chip(&sim);
        chip.write_sector(0x4000, &pattern(5), Duration::ZERO)
            .unwrap();
        assert_eq!(chip.sum, SumCheck::On);
        sim.borrow_mut().sum_bias = 0x0100;
        let programs = sim.borrow().programs.len();
        assert!(
            chip.write_sector(0x8000, &pattern(9), Duration::ZERO)
                .is_err()
        );
        assert_eq!(sim.borrow().programs.len(), programs);
    }

    #[test]
    fn unusable_checksum_disables_check_instead_of_failing() {
        let sim = Rc::new(RefCell::new(SimNor::dirty()));
        sim.borrow_mut().checksum = false;
        let mut chip = sim_chip(&sim);
        let target = pattern(13);
        chip.write_sector(0x4000, &target, Duration::ZERO).unwrap();
        assert_eq!(chip.sum, SumCheck::Off);
        let second = pattern(17);
        chip.write_sector(0x4400, &second, Duration::ZERO).unwrap();
        assert_eq!(chip.read_sector(0x4400).unwrap(), second);
    }

    #[test]
    fn short_sector_data_is_rejected() {
        let sim = Rc::new(RefCell::new(SimNor::dirty()));
        let mut chip = sim_chip(&sim);
        assert!(
            chip.write_sector(0x4000, &[0u8; 100], Duration::ZERO)
                .is_err()
        );
        assert!(sim.borrow().erases.is_empty());
    }

    #[test]
    fn flat_flash_erases_high_banks_and_protects_boot_area() {
        let sim = Rc::new(RefCell::new(SimNor::dirty()));
        sim.borrow_mut().mirrored = false;
        let mut chip = sim_chip(&sim);
        assert_eq!(chip.topology().unwrap(), Topology::Flat);
        let target = pattern(23);
        chip.write_sector(0x1E000, &target, Duration::ZERO).unwrap();
        assert_eq!(chip.read_sector(0x1E000).unwrap(), target);
        assert!(sim.borrow().erases.contains(&0x1E000));
        assert!(chip.erase_sector(0x3F800).is_err());
        assert!(!sim.borrow().erases.contains(&0x3F800));
    }

    #[test]
    fn whole_firmware_image_flashes_clean_on_flat_flash() {
        let sim = Rc::new(RefCell::new(SimNor::dirty()));
        sim.borrow_mut().mirrored = false;
        let boot = sim.borrow().peek(0x3F800, 0x800);
        let mut chip = sim_chip(&sim);
        let last = 0x3F800u32;
        for sector in (0..last).step_by(SECTOR) {
            chip.write_sector(sector, &pattern(sector as u8), Duration::ZERO)
                .unwrap();
        }
        for sector in (0..last).step_by(SECTOR) {
            assert_eq!(
                chip.read_sector(sector).unwrap(),
                pattern(sector as u8),
                "sector {sector:#x}"
            );
        }
        assert_eq!(sim.borrow().peek(0x3F800, 0x800), boot);
        assert_eq!(sim.borrow().erases.len(), (last / SECTOR as u32) as usize);
    }
}
