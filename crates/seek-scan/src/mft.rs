//! Pure (no I/O) parser for NTFS `FILE` records and data runs.

use seek_core::ATTR_DIRECTORY;

use crate::time::filetime_to_unix_ms;
use crate::win::FRN_MASK;
use crate::ScanError;

pub const FILE_MAGIC: &[u8; 4] = b"FILE";
pub const REC_USA_OFFSET: usize = 4;
pub const REC_USA_COUNT: usize = 6;
pub const REC_FIRST_ATTR: usize = 20;
pub const REC_FLAGS: usize = 22;
pub const REC_BASE_REF: usize = 32;
pub const FLAG_IN_USE: u16 = 0x1;
pub const FLAG_DIRECTORY: u16 = 0x2;

pub const ATTR_END: u32 = 0xFFFF_FFFF;
pub const ATTR_STANDARD_INFORMATION: u32 = 0x10;
pub const ATTR_FILE_NAME: u32 = 0x30;
pub const ATTR_DATA: u32 = 0x80;

pub const AH_LENGTH: usize = 4;
pub const AH_NON_RESIDENT: usize = 8;
pub const AH_NAME_LEN: usize = 9;
pub const AH_RES_VALUE_LEN: usize = 16;
pub const AH_RES_VALUE_OFF: usize = 20;
pub const AH_NR_START_VCN: usize = 16;
pub const AH_NR_RUNS_OFF: usize = 32;
pub const AH_NR_REAL_SIZE: usize = 48;

pub const SI_CREATED: usize = 0;
pub const SI_MODIFIED: usize = 8;
pub const SI_ATTRS: usize = 32;

pub const FN_PARENT: usize = 0;
pub const FN_NAME_LEN: usize = 64;
pub const FN_NAMESPACE: usize = 65;
pub const FN_NAME: usize = 66;
pub const NS_DOS: u8 = 2;

fn rd_u16(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
}
fn rd_u32(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}
fn rd_u64(b: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(o..o + 8)?.try_into().ok()?))
}

/// Verifies and removes the update-sequence ("fixup") markers of a FILE record in place.
/// `bytes_per_sector` is the fixup stride (512 for NTFS MFT records).
pub fn apply_fixup(record: &mut [u8], bytes_per_sector: usize) -> Result<(), ScanError> {
    let corrupt = |m: &str| ScanError::Corrupt(m.to_string());
    let usa_off = rd_u16(record, REC_USA_OFFSET).ok_or_else(|| corrupt("short record"))? as usize;
    let count = rd_u16(record, REC_USA_COUNT).ok_or_else(|| corrupt("short record"))? as usize;
    let usn = rd_u16(record, usa_off).ok_or_else(|| corrupt("bad update sequence offset"))?;
    for i in 1..count {
        let end = i * bytes_per_sector;
        let fix =
            rd_u16(record, usa_off + 2 * i).ok_or_else(|| corrupt("truncated fixup array"))?;
        if end > record.len() || end < 2 {
            return Err(corrupt("fixup beyond record"));
        }
        if rd_u16(record, end - 2) != Some(usn) {
            return Err(corrupt("fixup marker mismatch"));
        }
        record[end - 2..end].copy_from_slice(&fix.to_le_bytes());
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRecord {
    pub frn: u64,
    pub base_frn: u64,
    pub in_use: bool,
    pub is_dir: bool,
    /// (parent frn, name, namespace)
    pub name: Option<(u64, String, u8)>,
    pub created: i64,
    pub modified: i64,
    pub size: Option<u64>,
    pub attrs: u32,
    /// Only for the unnamed non-resident `$DATA` of record 0 (the MFT itself).
    pub data_runs: Option<Vec<(i64, u64)>>,
}

/// Parses a FILE record whose fixups have already been applied.
pub fn parse_record(frn: u64, rec: &[u8]) -> Result<ParsedRecord, ScanError> {
    if rec.len() < REC_BASE_REF + 8 || &rec[0..4] != FILE_MAGIC {
        return Err(ScanError::Corrupt("missing FILE magic".into()));
    }
    let flags = rd_u16(rec, REC_FLAGS).unwrap_or(0);
    let mut p = ParsedRecord {
        frn: frn & FRN_MASK,
        base_frn: rd_u64(rec, REC_BASE_REF).unwrap_or(0) & FRN_MASK,
        in_use: flags & FLAG_IN_USE != 0,
        is_dir: flags & FLAG_DIRECTORY != 0,
        name: None,
        created: 0,
        modified: 0,
        size: None,
        attrs: 0,
        data_runs: None,
    };
    let mut off = rd_u16(rec, REC_FIRST_ATTR).unwrap_or(0) as usize;
    while let Some(ty) = rd_u32(rec, off) {
        if ty == ATTR_END {
            break;
        }
        let len = rd_u32(rec, off + AH_LENGTH).unwrap_or(0) as usize;
        if len == 0 || off + len > rec.len() {
            break;
        }
        parse_attr(p.frn, &rec[off..off + len], ty, &mut p);
        off += len;
    }
    if p.is_dir {
        p.attrs |= ATTR_DIRECTORY;
    }
    Ok(p)
}

fn parse_attr(frn: u64, attr: &[u8], ty: u32, p: &mut ParsedRecord) -> Option<()> {
    let non_resident = *attr.get(AH_NON_RESIDENT)? != 0;
    let resident_value = || -> Option<&[u8]> {
        let vlen = rd_u32(attr, AH_RES_VALUE_LEN)? as usize;
        let voff = rd_u16(attr, AH_RES_VALUE_OFF)? as usize;
        attr.get(voff..voff + vlen)
    };
    match ty {
        ATTR_STANDARD_INFORMATION if !non_resident => {
            let v = resident_value()?;
            p.created = filetime_to_unix_ms(rd_u64(v, SI_CREATED)?);
            p.modified = filetime_to_unix_ms(rd_u64(v, SI_MODIFIED)?);
            p.attrs = rd_u32(v, SI_ATTRS)?;
        }
        ATTR_FILE_NAME if !non_resident => {
            let v = resident_value()?;
            let parent = rd_u64(v, FN_PARENT)? & FRN_MASK;
            let n = *v.get(FN_NAME_LEN)? as usize;
            let ns = *v.get(FN_NAMESPACE)?;
            let raw = v.get(FN_NAME..FN_NAME + 2 * n)?;
            let units: Vec<u16> = raw
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            let name = String::from_utf16_lossy(&units);
            // First non-DOS name wins; DOS only if nothing else is present.
            let replace = match &p.name {
                None => true,
                Some((_, _, cur_ns)) => *cur_ns == NS_DOS && ns != NS_DOS,
            };
            if replace {
                p.name = Some((parent, name, ns));
            }
        }
        ATTR_DATA if *attr.get(AH_NAME_LEN)? == 0 => {
            if non_resident {
                if rd_u64(attr, AH_NR_START_VCN)? == 0 {
                    p.size = Some(rd_u64(attr, AH_NR_REAL_SIZE)?);
                }
                if frn == 0 {
                    let runs_off = rd_u16(attr, AH_NR_RUNS_OFF)? as usize;
                    p.data_runs = Some(parse_data_runs(attr.get(runs_off..)?));
                }
            } else {
                p.size = Some(rd_u32(attr, AH_RES_VALUE_LEN)? as u64);
            }
        }
        _ => {}
    }
    Some(())
}

/// Decodes an NTFS data-run list into `(absolute lcn, clusters)`; sparse runs are skipped.
pub fn parse_data_runs(runs: &[u8]) -> Vec<(i64, u64)> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut lcn: i64 = 0;
    while let Some(&h) = runs.get(i) {
        if h == 0 {
            break;
        }
        let len_bytes = (h & 0xF) as usize;
        let off_bytes = (h >> 4) as usize;
        i += 1;
        let (Some(len_b), Some(off_b)) = (
            runs.get(i..i + len_bytes),
            runs.get(i + len_bytes..i + len_bytes + off_bytes),
        ) else {
            break;
        };
        i += len_bytes + off_bytes;
        if len_bytes == 0 || len_bytes > 8 || off_bytes > 8 {
            break;
        }
        let mut len = 0u64;
        for (k, b) in len_b.iter().enumerate() {
            len |= (*b as u64) << (8 * k);
        }
        if off_bytes == 0 {
            continue; // sparse
        }
        let mut delta = 0i64;
        for (k, b) in off_b.iter().enumerate() {
            delta |= (*b as i64) << (8 * k);
        }
        let shift = 64 - 8 * off_bytes as u32;
        delta = (delta << shift) >> shift; // sign-extend
        lcn += delta;
        out.push((lcn, len));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECTOR: usize = 512;
    const USA_OFF: usize = 48;

    fn resident_attr(ty: u32, value: &[u8]) -> Vec<u8> {
        let total = (24 + value.len()).div_ceil(8) * 8;
        let mut a = vec![0u8; total];
        a[0..4].copy_from_slice(&ty.to_le_bytes());
        a[4..8].copy_from_slice(&(total as u32).to_le_bytes());
        a[16..20].copy_from_slice(&(value.len() as u32).to_le_bytes());
        a[20..22].copy_from_slice(&24u16.to_le_bytes());
        a[24..24 + value.len()].copy_from_slice(value);
        a
    }

    fn filetime(unix_ms: i64) -> u64 {
        ((unix_ms + 11_644_473_600_000) * 10_000) as u64
    }

    fn file_name_attr(name: &str, parent: u64, ns: u8) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut v = vec![0u8; 66 + units.len() * 2];
        v[0..8].copy_from_slice(&parent.to_le_bytes());
        v[64] = units.len() as u8;
        v[65] = ns;
        for (i, u) in units.iter().enumerate() {
            v[66 + 2 * i..68 + 2 * i].copy_from_slice(&u.to_le_bytes());
        }
        resident_attr(ATTR_FILE_NAME, &v)
    }

    fn build_record(name: &str, parent: u64, dir: bool, size: u64, ns: u8) -> Vec<u8> {
        let mut rec = vec![0u8; 1024];
        rec[0..4].copy_from_slice(b"FILE");
        rec[4..6].copy_from_slice(&(USA_OFF as u16).to_le_bytes());
        rec[6..8].copy_from_slice(&3u16.to_le_bytes());
        rec[20..22].copy_from_slice(&56u16.to_le_bytes());
        let flags: u16 = 1 | if dir { 2 } else { 0 };
        rec[22..24].copy_from_slice(&flags.to_le_bytes());

        let mut si = vec![0u8; 72];
        si[0..8].copy_from_slice(&filetime(1_000_000).to_le_bytes());
        si[8..16].copy_from_slice(&filetime(2_000_000).to_le_bytes());
        si[32..36].copy_from_slice(&0x2u32.to_le_bytes()); // hidden
        let mut attrs = resident_attr(ATTR_STANDARD_INFORMATION, &si);
        if ns == 1 {
            attrs.extend(file_name_attr("DOSNAM~1", parent, NS_DOS));
        }
        attrs.extend(file_name_attr(name, parent, ns));
        attrs.extend(resident_attr(ATTR_DATA, &vec![0u8; size as usize]));
        attrs.extend(ATTR_END.to_le_bytes());
        rec[56..56 + attrs.len()].copy_from_slice(&attrs);

        // Marker bytes that the fixup must restore, then install the update sequence.
        rec[510..512].copy_from_slice(&[0xAA, 0xBB]);
        let usn = 0x0042u16;
        rec[USA_OFF..USA_OFF + 2].copy_from_slice(&usn.to_le_bytes());
        for i in 1..3 {
            let end = i * SECTOR;
            let orig = [rec[end - 2], rec[end - 1]];
            rec[USA_OFF + 2 * i..USA_OFF + 2 * i + 2].copy_from_slice(&orig);
            rec[end - 2..end].copy_from_slice(&usn.to_le_bytes());
        }
        rec
    }

    #[test]
    fn fixup_restores_and_rejects() {
        let mut rec = build_record("a.txt", 5, false, 10, 1);
        apply_fixup(&mut rec, SECTOR).unwrap();
        assert_eq!(&rec[510..512], &[0xAA, 0xBB]);
        assert_eq!(&rec[1022..1024], &[0, 0]);

        let mut bad = build_record("a.txt", 5, false, 10, 1);
        bad[1022] ^= 0xFF;
        assert!(matches!(
            apply_fixup(&mut bad, SECTOR),
            Err(ScanError::Corrupt(_))
        ));
    }

    #[test]
    fn parses_win32_name_over_dos() {
        let mut rec = build_record("Report.docx", 0xFFFF_0000_0000_0005, false, 77, 1);
        apply_fixup(&mut rec, SECTOR).unwrap();
        let p = parse_record(0x1234_0000_0000_0030, &rec).unwrap();
        assert_eq!(p.frn, 0x30);
        assert_eq!(p.base_frn, 0);
        assert!(p.in_use && !p.is_dir);
        assert_eq!(p.name, Some((5, "Report.docx".to_string(), 1)));
        assert_eq!(p.size, Some(77));
        assert_eq!(p.created, 1_000_000);
        assert_eq!(p.modified, 2_000_000);
        assert_eq!(p.attrs, 0x2);
        assert!(p.data_runs.is_none());
    }

    #[test]
    fn dir_flag_sets_attribute_and_dos_only_name_is_used() {
        let mut rec = build_record("SHORT~1", 5, true, 0, 2);
        apply_fixup(&mut rec, SECTOR).unwrap();
        let p = parse_record(40, &rec).unwrap();
        assert!(p.is_dir);
        assert_eq!(p.attrs & ATTR_DIRECTORY, ATTR_DIRECTORY);
        assert_eq!(p.name.unwrap().2, NS_DOS);
    }

    #[test]
    fn rejects_bad_magic() {
        assert!(parse_record(1, &[0u8; 1024]).is_err());
    }

    #[test]
    fn data_runs() {
        assert_eq!(
            parse_data_runs(&[0x21, 0x18, 0x34, 0x56, 0x00]),
            vec![(0x5634, 0x18)]
        );
        // second run: offset -0x10 relative to the first
        assert_eq!(
            parse_data_runs(&[0x21, 0x18, 0x34, 0x56, 0x11, 0x05, 0xF0, 0x00]),
            vec![(0x5634, 0x18), (0x5624, 0x05)]
        );
        // sparse run skipped, following offset still relative to the last real run
        assert_eq!(
            parse_data_runs(&[0x11, 0x04, 0x10, 0x01, 0x08, 0x11, 0x02, 0x05, 0x00]),
            vec![(0x10, 4), (0x15, 2)]
        );
    }
}
