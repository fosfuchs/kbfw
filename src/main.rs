mod chip;

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread::sleep;
use std::time::{Duration, Instant};

use chip::Chip;

const SECTOR: u32 = 0x400;
const CODE_END: u32 = 0x3C000;
const FACTORY_LAST: u32 = 0x3F800;
const FW_OFFSET: u32 = 0x100;
const CHIP_SIZE: u32 = 0x40000;
const FW_MAP_END: u32 = 0x3FD00;
const BLOCK: u32 = 56;
const FW_HEADER_LEN: usize = 0x100;
const FW_PAYLOAD_LEN: usize = 0x3FC00;
const FW_SEGMENTS: [u8; 24] = [
    0x00, 0xC0, 0x03, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0xC8, 0x03, 0x00, 0x00, 0x24, 0x00, 0x00,
    0x00, 0xF8, 0x03, 0x00, 0x00, 0x04, 0x00, 0x00,
];

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("test") => cmd_test(),
        Some("probe") => cmd_probe(),
        Some("read") => cmd_read(&args[1..]),
        Some("dump") => cmd_dump(&args[1..]),
        Some("pull") => cmd_pull(&args[1..]),
        Some("flash") => cmd_flash(&args[1..]),
        Some("verify") => cmd_verify(&args[1..]),
        Some("help") | Some("-h") | Some("--help") | None => {
            usage();
            0
        }
        Some(other) => {
            eprintln!("unknown command: {other}");
            usage();
            1
        }
    };
    ExitCode::from(code)
}

fn usage() {
    println!("kbfw - чтение и запись прошивки контроллера клавиатуры ITE (048d:89db) через hidraw");
    println!();
    println!("команды:");
    println!("  test                    проверка связи: чтение 0x000000 и 0x03FF00");
    println!("  probe                   чтение по контрольным адресам карты чипа");
    println!(
        "  read <адрес> [длина]    чтение блоками по 56 байт (hex; длина по умолчанию 0x1000)"
    );
    println!("  dump [файл]             полный дамп 0x40000 байт (по умолчанию kbfw-dump.bin)");
    println!("  pull [файл] [--model X] выкачка прошивки в формате FGA (по умолчанию kbfw-fw.bin)");
    println!("  flash <образ> [--dry] [--only 0x...,...] [--factory]");
    println!("  verify <образ> [--factory]");
    println!();
    println!("работает, только когда контроллер в режиме загрузчика 048d:89db");
}

fn open_chip() -> Result<Chip, u8> {
    match Chip::open() {
        Ok(chip) => {
            println!("device = {}", chip.path.display());
            Ok(chip)
        }
        Err(e) => {
            eprintln!("kbfw: {e}");
            Err(1)
        }
    }
}

fn cmd_test() -> u8 {
    let mut chip = match open_chip() {
        Ok(chip) => chip,
        Err(code) => return code,
    };
    match chip.read_block(0, BLOCK as usize) {
        Ok(d) => println!("read@0x000000: {}", hex(&d)),
        Err(e) => println!("read@0x000000: FAILED ({e})"),
    }
    match chip.read_block(0x3FF00, BLOCK as usize) {
        Ok(d) => println!("read@0x03FF00: {}", hex(&d)),
        Err(e) => println!("read@0x03FF00: FAILED ({e})"),
    }
    0
}

fn cmd_probe() -> u8 {
    let mut chip = match open_chip() {
        Ok(chip) => chip,
        Err(code) => return code,
    };
    let probes: [u32; 16] = [
        0x0, 0x1000, 0x2000, 0x10000, 0x1E000, 0x20000, 0x30000, 0x33B80, 0x33BC0, 0x34000,
        0x35000, 0x3F000, 0x3FF00, 0x1000, 0x2000, 0x0,
    ];
    for a in probes {
        match chip.read_block(a, BLOCK as usize) {
            Ok(d) => println!("{a:#07x}: {}", hex(&d)),
            Err(_) => println!("{a:#07x}: FAIL"),
        }
    }
    0
}

fn cmd_read(args: &[String]) -> u8 {
    let start = args.first().and_then(|s| parse_hex(s)).unwrap_or(0);
    let length = args.get(1).and_then(|s| parse_hex(s)).unwrap_or(0x1000);
    let mut chip = match open_chip() {
        Ok(chip) => chip,
        Err(code) => return code,
    };
    let mut failed = 0u32;
    let mut a = start;
    while a < start + length {
        match chip.read_block(a, BLOCK as usize) {
            Ok(d) => println!("{a:#07x}: {}", hex(&d)),
            Err(_) => {
                failed += 1;
                println!("{a:#07x}: FAIL");
            }
        }
        a += BLOCK;
    }
    println!("failed: {failed}");
    0
}

fn cmd_dump(args: &[String]) -> u8 {
    let out = args
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("kbfw-dump.bin"));
    let mut chip = match open_chip() {
        Ok(chip) => chip,
        Err(code) => return code,
    };
    let (img, badpos) = read_chip_full(&mut chip);
    if let Err(e) = fs::write(&out, &img) {
        eprintln!("cannot save {}: {e}", out.display());
        return 1;
    }
    let bad_path = out.with_extension("bad.txt");
    let bad_text: Vec<String> = badpos.iter().map(|a| format!("{a:#x}")).collect();
    let _ = fs::write(&bad_path, bad_text.join("\n"));
    println!("saved {} failed blocks: {}", out.display(), badpos.len());
    let names = firmware_names(&img);
    println!(
        "firmware names: {:?}",
        names.iter().take(10).collect::<Vec<_>>()
    );
    let ff = img.iter().filter(|&&b| b == 0xFF).count();
    println!("blank 0xFF: {:.1}%", 100.0 * ff as f64 / img.len() as f64);
    0
}

fn cmd_pull(args: &[String]) -> u8 {
    let mut out: Option<PathBuf> = None;
    let mut model_arg: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--model" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("--model требует значение (например G614PP)");
                    return 1;
                }
                model_arg = Some(args[i].clone());
            }
            other if other.starts_with("--") => {
                eprintln!("unknown flag: {other}");
                return 1;
            }
            other => {
                if out.is_some() {
                    eprintln!("лишний аргумент: {other}");
                    return 1;
                }
                out = Some(PathBuf::from(other));
            }
        }
        i += 1;
    }
    let out = out.unwrap_or_else(|| PathBuf::from("kbfw-fw.bin"));
    let mut chip = match open_chip() {
        Ok(chip) => chip,
        Err(code) => return code,
    };
    let (img, badpos) = read_chip_full(&mut chip);
    let meta = chip_metadata_string(&img);
    match &meta {
        Some(s) => println!("метаданные чипа: {s}"),
        None => println!("метаданные чипа: не распознаны"),
    }
    let model = model_arg.or_else(|| meta.as_deref().and_then(parse_model));
    if model.is_none() {
        eprintln!("предупреждение: модель не определена, шапка без имени (задать --model)");
    }
    let fw_img = build_firmware_image(&img, model.as_deref());
    if let Err(e) = fs::write(&out, &fw_img) {
        eprintln!("cannot save {}: {e}", out.display());
        return 1;
    }
    if !badpos.is_empty() {
        let bad_path = out.with_extension("bad.txt");
        let text: Vec<String> = badpos.iter().map(|a| format!("{a:#x}")).collect();
        let _ = fs::write(&bad_path, text.join("\n"));
        println!(
            "внимание: {} сбойных блоков, список {}; образ может быть неполным",
            badpos.len(),
            bad_path.display()
        );
    }
    println!(
        "образ: {} ({} байт, шапка $INVENTEC, модель {})",
        out.display(),
        fw_img.len(),
        model.as_deref().unwrap_or("-")
    );
    println!("прошить обратно: kbfw flash {}", out.display());
    if badpos.is_empty() { 0 } else { 1 }
}

fn read_chip_full(chip: &mut Chip) -> (Vec<u8>, Vec<u32>) {
    let size = CHIP_SIZE as usize;
    let mut img = vec![0u8; size];
    let mut badrun = 0u64;
    let mut badpos: Vec<u32> = Vec::new();
    let t0 = Instant::now();
    let mut a = 0u32;
    while (a as usize) < size {
        match chip.read_block(a, BLOCK as usize) {
            Ok(d) => {
                let end = (a as usize + d.len()).min(size);
                img[a as usize..end].copy_from_slice(&d[..end - a as usize]);
                badrun = 0;
            }
            Err(_) => {
                badpos.push(a);
                badrun += 1;
                if badrun >= 40 && chip.reopen() {
                    badrun = 0;
                    println!("reopened {}", chip.path.display());
                }
            }
        }
        if a.is_multiple_of(0x8000) {
            println!(
                "{a:#07x}/{size:#07x} bad={} {}s",
                badpos.len(),
                t0.elapsed().as_secs()
            );
        }
        a += BLOCK;
    }
    (img, badpos)
}

fn build_firmware_image(chip_img: &[u8], model: Option<&str>) -> Vec<u8> {
    let mut out = vec![0u8; FW_HEADER_LEN + FW_PAYLOAD_LEN];
    out[..9].copy_from_slice(b"$INVENTEC");
    out[0x10..0x28].copy_from_slice(&FW_SEGMENTS);
    out[0xE0..0xE8].copy_from_slice(b"00.00.01");
    out[0xEC..0xF0].copy_from_slice(&1u32.to_le_bytes());
    if let Some(m) = model {
        let bytes = m.as_bytes();
        let n = bytes.len().min(16);
        out[0xF0..0xF0 + n].copy_from_slice(&bytes[..n]);
    }
    let n = chip_img.len().min(FW_PAYLOAD_LEN);
    out[FW_HEADER_LEN..FW_HEADER_LEN + n].copy_from_slice(&chip_img[..n]);
    out
}

fn chip_metadata_string(img: &[u8]) -> Option<String> {
    let zone = img.get(0x3C000..0x3C100)?;
    let start = (0..zone.len().saturating_sub(2))
        .find(|&i| zone[i] == b'F' && zone[i + 1] == b'G' && zone[i + 2] == b'A')?;
    let tail = &zone[start..];
    let end = tail.iter().position(|&b| b == 0).unwrap_or(tail.len());
    let s = std::str::from_utf8(&tail[..end]).ok()?;
    if s.matches('.').count() == 2 {
        Some(s.to_string())
    } else {
        None
    }
}

fn parse_model(meta: &str) -> Option<String> {
    let parts: Vec<&str> = meta.split('.').collect();
    if parts.len() == 3 && !parts[1].is_empty() && parts[1].len() <= 16 {
        Some(parts[1].to_string())
    } else {
        None
    }
}

fn cmd_flash(args: &[String]) -> u8 {
    let mut fw_path: Option<PathBuf> = None;
    let mut dry = false;
    let mut factory = false;
    let mut only: Option<Vec<u32>> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dry" => dry = true,
            "--factory" => factory = true,
            "--only" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("--only требует список секторов (например 0x11000,0x11400)");
                    return 1;
                }
                match parse_sector_list(&args[i]) {
                    Some(list) => only = Some(list),
                    None => {
                        eprintln!("плохой список секторов: {}", args[i]);
                        return 1;
                    }
                }
            }
            other if other.starts_with("--") => {
                eprintln!("unknown flag: {other}");
                return 1;
            }
            other => {
                if fw_path.is_some() {
                    eprintln!("лишний аргумент: {other}");
                    return 1;
                }
                fw_path = Some(PathBuf::from(other));
            }
        }
        i += 1;
    }
    let Some(fw_path) = fw_path else {
        eprintln!("usage: kbfw flash <образ> [--dry] [--only 0x...,...] [--factory]");
        return 1;
    };
    let fw = match fs::read(&fw_path) {
        Ok(fw) => fw,
        Err(e) => {
            eprintln!("cannot read {}: {e}", fw_path.display());
            return 1;
        }
    };
    if fw.len() < FW_MAP_END as usize {
        eprintln!(
            "предупреждение: образ короче карты чипа ({FW_MAP_END:#x}), хвостовые сектора будут пропущены"
        );
    }
    let mut chip = match open_chip() {
        Ok(chip) => chip,
        Err(code) => return code,
    };
    let sectors = match only {
        Some(list) => list,
        None => match scan_diff(&mut chip, &fw, factory) {
            Ok(todo) => todo,
            Err(e) => {
                eprintln!("scan error: {e}");
                return 1;
            }
        },
    };
    let total = sectors.len();
    println!("нужно записать: {total} секторов");
    if dry {
        let preview: Vec<String> = sectors.iter().take(20).map(|s| format!("{s:#x}")).collect();
        println!("dry: {preview:?}");
        return 0;
    }
    let t0 = Instant::now();
    let mut done = 0u32;
    let mut failed = 0u32;
    let mut still: Vec<u32> = Vec::new();
    for (idx, s) in sectors.iter().enumerate() {
        let target = match target_slice(&fw, *s) {
            Some(t) => t,
            None => {
                failed += 1;
                still.push(*s);
                println!("[{}/{}] {:#07x} FAIL (вне карты образа)", idx + 1, total, s);
                continue;
            }
        };
        let mut ok = false;
        let mut lost = false;
        for _ in 0..4 {
            if chip
                .write_sector(*s, target, Duration::from_millis(5))
                .is_err()
            {
                if !chip.reopen() {
                    lost = true;
                    break;
                }
                sleep(Duration::from_millis(300));
                continue;
            }
            sleep(Duration::from_millis(50));
            match chip.read_sector(*s) {
                Ok(r) if r == target => {
                    ok = true;
                    break;
                }
                Ok(_) => {}
                Err(_) => {
                    if !chip.reopen() {
                        lost = true;
                        break;
                    }
                    sleep(Duration::from_millis(300));
                }
            }
        }
        if ok {
            done += 1;
            println!(
                "[{}/{}] {:#07x} OK {}s",
                idx + 1,
                total,
                s,
                t0.elapsed().as_secs()
            );
        } else {
            failed += 1;
            still.push(*s);
            if lost {
                println!("!! устройство пропало");
            }
            println!("[{}/{}] {:#07x} FAIL", idx + 1, total, s);
        }
    }
    println!(
        "--- итог: записано {done}, провалено {failed}, время {}s",
        t0.elapsed().as_secs()
    );
    if !still.is_empty() {
        let text: Vec<String> = still.iter().map(|s| format!("{s:#x}")).collect();
        let _ = fs::write("kbfw-still.txt", text.join("\n"));
        println!("остались: {text:?}");
        println!("список: kbfw-still.txt");
    }
    if failed > 0 { 1 } else { 0 }
}

fn cmd_verify(args: &[String]) -> u8 {
    let mut fw_path: Option<PathBuf> = None;
    let mut factory = false;
    for arg in args {
        match arg.as_str() {
            "--factory" => factory = true,
            other if other.starts_with("--") => {
                eprintln!("unknown flag: {other}");
                return 1;
            }
            other => fw_path = Some(PathBuf::from(other)),
        }
    }
    let Some(fw_path) = fw_path else {
        eprintln!("usage: kbfw verify <образ> [--factory]");
        return 1;
    };
    let fw = match fs::read(&fw_path) {
        Ok(fw) => fw,
        Err(e) => {
            eprintln!("cannot read {}: {e}", fw_path.display());
            return 1;
        }
    };
    let mut chip = match open_chip() {
        Ok(chip) => chip,
        Err(code) => return code,
    };
    let sectors = sector_range(factory);
    let mut mismatched: Vec<u32> = Vec::new();
    let mut checked = 0usize;
    for (idx, s) in sectors.iter().enumerate() {
        let Some(target) = target_slice(&fw, *s) else {
            continue;
        };
        let r = match chip.read_sector(*s) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("read error at {s:#x}: {e}");
                return 1;
            }
        };
        if r != target {
            mismatched.push(*s);
        }
        checked += 1;
        if idx.is_multiple_of(48) || idx + 1 == sectors.len() {
            println!(
                "verify {}/{}, mismatches={}",
                idx + 1,
                sectors.len(),
                mismatched.len()
            );
        }
    }
    if mismatched.is_empty() {
        println!("все {checked} секторов совпадают с образом");
        0
    } else {
        println!("несовпадающих секторов: {}", mismatched.len());
        for s in &mismatched {
            println!("{s:#07x}");
        }
        1
    }
}

fn scan_diff(chip: &mut Chip, fw: &[u8], factory: bool) -> io::Result<Vec<u32>> {
    let all = sector_range(factory);
    let mut todo: Vec<u32> = Vec::new();
    for (idx, s) in all.iter().enumerate() {
        let Some(target) = target_slice(fw, *s) else {
            continue;
        };
        let r = match chip.read_sector(*s) {
            Ok(r) => r,
            Err(_) => {
                if !chip.reopen() {
                    return Err(io::Error::other("device lost during scan"));
                }
                chip.read_sector(*s)?
            }
        };
        if r != target {
            todo.push(*s);
        }
        if idx.is_multiple_of(48) || idx + 1 == all.len() {
            println!("scan {}/{}, todo={}", idx + 1, all.len(), todo.len());
        }
    }
    Ok(todo)
}

fn sector_range(factory: bool) -> Vec<u32> {
    let mut sectors: Vec<u32> = (0..CODE_END).step_by(SECTOR as usize).collect();
    if factory {
        sectors.extend((CODE_END..=FACTORY_LAST).step_by(SECTOR as usize));
    }
    sectors
}

fn target_slice(fw: &[u8], sector: u32) -> Option<&[u8]> {
    let start = sector as usize + FW_OFFSET as usize;
    let end = start + SECTOR as usize;
    if end <= fw.len() {
        Some(&fw[start..end])
    } else {
        None
    }
}

fn parse_hex(s: &str) -> Option<u32> {
    let t = s.trim();
    let t = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    u32::from_str_radix(t, 16).ok()
}

fn parse_sector_list(s: &str) -> Option<Vec<u32>> {
    s.split(',').map(parse_hex).collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn firmware_names(img: &[u8]) -> Vec<String> {
    let mut names = BTreeSet::new();
    for i in 0..img.len().saturating_sub(2) {
        if img[i] != b'F' || img[i + 1] != b'G' {
            continue;
        }
        let mut j = i;
        while j < img.len() && j - i < 32 {
            let b = img[j];
            if b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'.' {
                j += 1;
            } else {
                break;
            }
        }
        if let Ok(candidate) = std::str::from_utf8(&img[i..j]) {
            let parts: Vec<&str> = candidate.split('.').collect();
            if parts.len() == 3
                && (3..=10).contains(&parts[0].len())
                && !parts[1].is_empty()
                && !parts[2].is_empty()
            {
                names.insert(candidate.to_string());
            }
        }
    }
    names.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_forms() {
        assert_eq!(parse_hex("0x3C000"), Some(0x3C000));
        assert_eq!(parse_hex("3c000"), Some(0x3C000));
        assert_eq!(parse_hex("zz"), None);
    }

    #[test]
    fn sector_ranges() {
        let code = sector_range(false);
        assert_eq!(code.len(), 240);
        assert_eq!(code[0], 0);
        assert_eq!(*code.last().unwrap(), 0x3BC00);
        let full = sector_range(true);
        assert_eq!(full.len(), 255);
        assert_eq!(*full.last().unwrap(), 0x3F800);
    }

    #[test]
    fn target_slice_map() {
        let fw = vec![0u8; FW_MAP_END as usize];
        assert!(target_slice(&fw, 0x3F800).is_some());
        assert!(target_slice(&fw, 0x3FC00).is_none());
    }

    #[test]
    fn names_found() {
        let mut img = vec![0u8; 0x1000];
        let name = b"FGA00000.G614PP.317";
        img[0x100..0x100 + name.len()].copy_from_slice(name);
        let names = firmware_names(&img);
        assert_eq!(names, vec!["FGA00000.G614PP.317".to_string()]);
    }

    #[test]
    fn segment_table_matches_official_corpus() {
        assert_eq!(
            hex(&FW_SEGMENTS),
            "00c003000004000000c803000024000000f8030000040000"
        );
    }

    #[test]
    fn firmware_header_layout() {
        let mut chip = vec![0u8; CHIP_SIZE as usize];
        chip[0] = 0xAA;
        chip[0x2AC] = 0xBB;
        chip[FW_PAYLOAD_LEN - 1] = 0xCC;
        chip[CHIP_SIZE as usize - 1] = 0xDD;
        let fw = build_firmware_image(&chip, Some("G614PP"));
        assert_eq!(fw.len(), FW_MAP_END as usize);
        let mut head = vec![0u8; 0x28];
        head[..9].copy_from_slice(b"$INVENTEC");
        head[0x10..0x28].copy_from_slice(&FW_SEGMENTS);
        assert_eq!(&fw[..0x28], &head[..]);
        let mut tail = vec![0u8; 0x20];
        tail[..8].copy_from_slice(b"00.00.01");
        tail[0xC..0x10].copy_from_slice(&1u32.to_le_bytes());
        tail[0x10..0x16].copy_from_slice(b"G614PP");
        assert_eq!(&fw[0xE0..0x100], &tail[..]);
        assert_eq!(fw[0x100], 0xAA);
        assert_eq!(fw[0x100 + 0x2AC], 0xBB);
        assert_eq!(fw[FW_MAP_END as usize - 1], 0xCC);
        assert!(!fw.contains(&0xDD));
    }

    #[test]
    fn pull_round_trip_via_flash_map() {
        let mut chip = vec![0u8; CHIP_SIZE as usize];
        for (i, b) in chip.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        chip[0x3C000..0x3C400].fill(0);
        let name = b"FGA00000.G614PP.318";
        chip[0x3C000..0x3C000 + name.len()].copy_from_slice(name);
        let meta = chip_metadata_string(&chip);
        assert_eq!(meta.as_deref(), Some("FGA00000.G614PP.318"));
        let model = meta.as_deref().and_then(parse_model);
        assert_eq!(model.as_deref(), Some("G614PP"));
        let fw = build_firmware_image(&chip, model.as_deref());
        for s in sector_range(false) {
            assert_eq!(
                target_slice(&fw, s),
                Some(&chip[s as usize..s as usize + SECTOR as usize])
            );
        }
    }
}
