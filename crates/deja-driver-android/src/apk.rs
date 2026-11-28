//! Pure-Rust, zero-`unsafe`, zero-external-crate Android `.apk` package ID extractor.
//!
//! Reads only the ZIP End-of-Central-Directory, Central Directory, and the `AndroidManifest.xml`
//! entry from an APK file (supporting both Store method `0` and Deflate method `8` via a bounded
//! RFC 1951 inflater), then decodes the Android Binary XML (AXML) string pool and root
//! `<manifest package="...">` attribute.

use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::Path;

use crate::adb::validate_package_id;
use crate::error::{AndroidDriverError, Result};
use crate::process::open_read_nofollow;

const ZIP_LOCAL_FILE_HEADER_SIG: u32 = 0x0403_4b50;
const ZIP_CENTRAL_DIR_HEADER_SIG: u32 = 0x0201_4b50;
const ZIP_EOCD_SIG: u32 = 0x0605_4b50;
const ZIP64_EOCD_LOCATOR_SIG: u32 = 0x0706_4b50;
const ZIP64_EOCD_SIG: u32 = 0x0606_4b50;
const ZIP_METHOD_STORE: u16 = 0;
const ZIP_METHOD_DEFLATE: u16 = 8;

const MAX_EOCD_SEARCH_BYTES: u64 = 22 + 65_535;
const MAX_CENTRAL_DIR_BYTES: usize = 32 * 1024 * 1024;
const MAX_MANIFEST_BYTES: usize = 4 * 1024 * 1024;

const RES_STRING_POOL_TYPE: u16 = 0x0001;
const RES_XML_TYPE: u16 = 0x0003;
const RES_XML_START_ELEMENT_TYPE: u16 = 0x0102;
const STRING_POOL_UTF8_FLAG: u32 = 1 << 8;
const AXML_NO_INDEX: u32 = 0xFFFF_FFFF;
const AXML_TYPE_STRING: u8 = 0x03;

/// Extracts and validates the `<manifest package="...">` identifier from in-memory APK bytes.
pub fn extract_apk_package_id(apk_bytes: &[u8]) -> Result<String> {
    let mut cursor = Cursor::new(apk_bytes);
    extract_apk_package_id_from_reader(&mut cursor, apk_bytes.len() as u64)
}

/// Extracts and validates the `<manifest package="...">` identifier from an on-disk regular,
/// non-symlink `.apk` file (`O_NOFOLLOW`) without reading the entire archive into memory.
pub fn extract_apk_package_id_from_path(apk_path: &Path) -> Result<String> {
    let mut file = open_read_nofollow(apk_path).map_err(|err| {
        AndroidDriverError::InvalidPath(format!(
            "Android APK must be an existing regular non-symlink file ({}): {err}",
            apk_path.display()
        ))
    })?;
    let meta = file.metadata().map_err(|source| AndroidDriverError::Io {
        context: format!("stat Android APK {}", apk_path.display()),
        source,
    })?;
    if !meta.is_file() {
        return Err(AndroidDriverError::InvalidPath(format!(
            "Android APK path is not a regular file: {}",
            apk_path.display()
        )));
    }
    extract_apk_package_id_from_reader(&mut file, meta.len())
}

fn extract_apk_package_id_from_reader<R: Read + Seek>(
    reader: &mut R,
    file_len: u64,
) -> Result<String> {
    let manifest_axml = read_zip_entry(reader, file_len, "AndroidManifest.xml")?;
    parse_axml_manifest_package(&manifest_axml)
}

struct ZipEntryLoc {
    method: u16,
    compressed_size: u64,
    uncompressed_size: u64,
    local_header_offset: u64,
}

fn read_u16_le(bytes: &[u8], offset: usize) -> Option<u16> {
    let slice = bytes.get(offset..offset + 2)?;
    Some(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_u32_le(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset + 4)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn read_u64_le(bytes: &[u8], offset: usize) -> Option<u64> {
    let slice = bytes.get(offset..offset + 8)?;
    Some(u64::from_le_bytes([
        slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
    ]))
}

fn locate_central_directory<R: Read + Seek>(reader: &mut R, file_len: u64) -> Result<(u64, u64)> {
    if file_len < 22 {
        return Err(AndroidDriverError::InvalidPath(
            "file is too small to be a valid Android APK/ZIP archive".to_string(),
        ));
    }
    let tail_len = file_len.min(MAX_EOCD_SEARCH_BYTES) as usize;
    let tail_start = file_len - tail_len as u64;
    reader
        .seek(SeekFrom::Start(tail_start))
        .map_err(|source| AndroidDriverError::Io {
            context: "seek to ZIP End-of-Central-Directory".to_string(),
            source,
        })?;
    let mut tail = vec![0u8; tail_len];
    reader
        .read_exact(&mut tail)
        .map_err(|source| AndroidDriverError::Io {
            context: "read ZIP End-of-Central-Directory".to_string(),
            source,
        })?;

    // Scan backwards for the End-of-Central-Directory record whose comment length reaches EOF.
    let mut eocd_rel = None;
    let mut pos = tail.len() - 22;
    loop {
        if read_u32_le(&tail, pos) == Some(ZIP_EOCD_SIG) {
            let comment_len = read_u16_le(&tail, pos + 20).unwrap_or(0) as usize;
            if pos + 22 + comment_len == tail.len() {
                eocd_rel = Some(pos);
                break;
            }
        }
        if pos == 0 {
            break;
        }
        pos -= 1;
    }
    let Some(eocd_idx) = eocd_rel else {
        return Err(AndroidDriverError::InvalidPath(
            "APK archive is missing a valid ZIP End-of-Central-Directory record".to_string(),
        ));
    };

    let mut cd_size = u64::from(read_u32_le(&tail, eocd_idx + 12).unwrap_or(0));
    let mut cd_offset = u64::from(read_u32_le(&tail, eocd_idx + 16).unwrap_or(0));

    // Handle ZIP64 EOCD when standard 32-bit fields saturate.
    if (cd_size == u64::from(u32::MAX) || cd_offset == u64::from(u32::MAX)) && eocd_idx >= 20 {
        let loc_idx = eocd_idx - 20;
        if read_u32_le(&tail, loc_idx) == Some(ZIP64_EOCD_LOCATOR_SIG) {
            let z64_offset = read_u64_le(&tail, loc_idx + 8).unwrap_or(u64::MAX);
            if z64_offset.saturating_add(56) <= file_len {
                reader.seek(SeekFrom::Start(z64_offset)).map_err(|source| {
                    AndroidDriverError::Io {
                        context: "seek to ZIP64 End-of-Central-Directory".to_string(),
                        source,
                    }
                })?;
                let mut z64 = [0u8; 56];
                reader
                    .read_exact(&mut z64)
                    .map_err(|source| AndroidDriverError::Io {
                        context: "read ZIP64 End-of-Central-Directory".to_string(),
                        source,
                    })?;
                if read_u32_le(&z64, 0) == Some(ZIP64_EOCD_SIG) {
                    cd_size = read_u64_le(&z64, 40).unwrap_or(cd_size);
                    cd_offset = read_u64_le(&z64, 48).unwrap_or(cd_offset);
                }
            }
        }
    }

    if cd_size > MAX_CENTRAL_DIR_BYTES as u64 {
        return Err(AndroidDriverError::InvalidPath(format!(
            "APK ZIP central directory size ({cd_size} bytes) exceeds {MAX_CENTRAL_DIR_BYTES}-byte safety limit"
        )));
    }
    if cd_offset.saturating_add(cd_size) > file_len {
        return Err(AndroidDriverError::InvalidPath(
            "APK ZIP central directory bounds exceed archive size".to_string(),
        ));
    }

    Ok((cd_offset, cd_size))
}

fn find_central_dir_entry(cd: &[u8], target_name: &str) -> Result<ZipEntryLoc> {
    let mut offset = 0usize;
    while offset < cd.len() {
        if offset + 46 > cd.len() {
            return Err(AndroidDriverError::InvalidPath(
                "truncated APK ZIP central directory header".to_string(),
            ));
        }
        if read_u32_le(cd, offset) != Some(ZIP_CENTRAL_DIR_HEADER_SIG) {
            return Err(AndroidDriverError::InvalidPath(
                "invalid APK ZIP central directory header signature".to_string(),
            ));
        }
        let method = read_u16_le(cd, offset + 10).unwrap_or(u16::MAX);
        let mut comp_size = u64::from(read_u32_le(cd, offset + 20).unwrap_or(0));
        let mut uncomp_size = u64::from(read_u32_le(cd, offset + 24).unwrap_or(0));
        let name_len = read_u16_le(cd, offset + 28).unwrap_or(0) as usize;
        let extra_len = read_u16_le(cd, offset + 30).unwrap_or(0) as usize;
        let comment_len = read_u16_le(cd, offset + 32).unwrap_or(0) as usize;
        let mut local_offset = u64::from(read_u32_le(cd, offset + 42).unwrap_or(0));

        let name_start = offset + 46;
        let extra_start = name_start + name_len;
        let next_offset = extra_start + extra_len + comment_len;
        if next_offset > cd.len() {
            return Err(AndroidDriverError::InvalidPath(
                "truncated APK ZIP central directory entry metadata".to_string(),
            ));
        }

        let entry_name = &cd[name_start..extra_start];
        if entry_name == target_name.as_bytes() {
            // Decode ZIP64 extended information extra field (`0x0001`) if any 32-bit field saturated.
            if uncomp_size == u64::from(u32::MAX)
                || comp_size == u64::from(u32::MAX)
                || local_offset == u64::from(u32::MAX)
            {
                parse_zip64_extra(
                    &cd[extra_start..extra_start + extra_len],
                    &mut uncomp_size,
                    &mut comp_size,
                    &mut local_offset,
                );
            }
            return Ok(ZipEntryLoc {
                method,
                compressed_size: comp_size,
                uncompressed_size: uncomp_size,
                local_header_offset: local_offset,
            });
        }
        offset = next_offset;
    }

    Err(AndroidDriverError::InvalidPath(format!(
        "APK archive does not contain `{target_name}`"
    )))
}

fn parse_zip64_extra(
    extra: &[u8],
    uncomp_size: &mut u64,
    comp_size: &mut u64,
    local_offset: &mut u64,
) {
    let mut pos = 0usize;
    while pos + 4 <= extra.len() {
        let header_id = read_u16_le(extra, pos).unwrap_or(0);
        let data_size = read_u16_le(extra, pos + 2).unwrap_or(0) as usize;
        let data_start = pos + 4;
        if data_start + data_size > extra.len() {
            break;
        }
        if header_id == 0x0001 {
            let mut cursor = data_start;
            let data_end = data_start + data_size;
            if *uncomp_size == u64::from(u32::MAX) && cursor + 8 <= data_end {
                *uncomp_size = read_u64_le(extra, cursor).unwrap_or(*uncomp_size);
                cursor += 8;
            }
            if *comp_size == u64::from(u32::MAX) && cursor + 8 <= data_end {
                *comp_size = read_u64_le(extra, cursor).unwrap_or(*comp_size);
                cursor += 8;
            }
            if *local_offset == u64::from(u32::MAX) && cursor + 8 <= data_end {
                *local_offset = read_u64_le(extra, cursor).unwrap_or(*local_offset);
            }
            break;
        }
        pos = data_start + data_size;
    }
}

fn read_zip_entry<R: Read + Seek>(
    reader: &mut R,
    file_len: u64,
    entry_name: &str,
) -> Result<Vec<u8>> {
    let (cd_offset, cd_size) = locate_central_directory(reader, file_len)?;
    reader
        .seek(SeekFrom::Start(cd_offset))
        .map_err(|source| AndroidDriverError::Io {
            context: "seek to APK ZIP central directory".to_string(),
            source,
        })?;
    let mut cd = vec![0u8; cd_size as usize];
    reader
        .read_exact(&mut cd)
        .map_err(|source| AndroidDriverError::Io {
            context: "read APK ZIP central directory".to_string(),
            source,
        })?;

    let loc = find_central_dir_entry(&cd, entry_name)?;
    if loc.uncompressed_size > MAX_MANIFEST_BYTES as u64 {
        return Err(AndroidDriverError::InvalidPath(format!(
            "APK `{entry_name}` uncompressed size ({} bytes) exceeds {MAX_MANIFEST_BYTES}-byte limit",
            loc.uncompressed_size
        )));
    }
    if loc.compressed_size > MAX_MANIFEST_BYTES as u64 {
        return Err(AndroidDriverError::InvalidPath(format!(
            "APK `{entry_name}` compressed size ({} bytes) exceeds {MAX_MANIFEST_BYTES}-byte limit",
            loc.compressed_size
        )));
    }
    if loc.local_header_offset.saturating_add(30) > file_len {
        return Err(AndroidDriverError::InvalidPath(
            "APK local file header offset exceeds archive size".to_string(),
        ));
    }

    reader
        .seek(SeekFrom::Start(loc.local_header_offset))
        .map_err(|source| AndroidDriverError::Io {
            context: format!("seek to APK local header for {entry_name}"),
            source,
        })?;
    let mut lfh = [0u8; 30];
    reader
        .read_exact(&mut lfh)
        .map_err(|source| AndroidDriverError::Io {
            context: format!("read APK local header for {entry_name}"),
            source,
        })?;
    if read_u32_le(&lfh, 0) != Some(ZIP_LOCAL_FILE_HEADER_SIG) {
        return Err(AndroidDriverError::InvalidPath(format!(
            "invalid ZIP local file header signature for `{entry_name}`"
        )));
    }
    let lfh_name_len = u64::from(read_u16_le(&lfh, 26).unwrap_or(0));
    let lfh_extra_len = u64::from(read_u16_le(&lfh, 28).unwrap_or(0));
    let data_offset = loc
        .local_header_offset
        .saturating_add(30)
        .saturating_add(lfh_name_len)
        .saturating_add(lfh_extra_len);
    if data_offset.saturating_add(loc.compressed_size) > file_len {
        return Err(AndroidDriverError::InvalidPath(format!(
            "APK `{entry_name}` payload extends beyond archive size"
        )));
    }

    reader
        .seek(SeekFrom::Start(data_offset))
        .map_err(|source| AndroidDriverError::Io {
            context: format!("seek to APK payload for {entry_name}"),
            source,
        })?;
    let mut raw = vec![0u8; loc.compressed_size as usize];
    reader
        .read_exact(&mut raw)
        .map_err(|source| AndroidDriverError::Io {
            context: format!("read APK payload for {entry_name}"),
            source,
        })?;

    match loc.method {
        ZIP_METHOD_STORE => {
            if raw.len() as u64 != loc.uncompressed_size {
                return Err(AndroidDriverError::InvalidPath(format!(
                    "APK stored `{entry_name}` size mismatch ({} vs {})",
                    raw.len(),
                    loc.uncompressed_size
                )));
            }
            Ok(raw)
        }
        ZIP_METHOD_DEFLATE => {
            let cap = (loc.uncompressed_size as usize).min(MAX_MANIFEST_BYTES);
            let out = inflate_raw(&raw, MAX_MANIFEST_BYTES)?;
            if out.len() as u64 != loc.uncompressed_size && cap > 0 {
                return Err(AndroidDriverError::InvalidPath(format!(
                    "APK deflated `{entry_name}` size mismatch (inflated {} vs declared {})",
                    out.len(),
                    loc.uncompressed_size
                )));
            }
            Ok(out)
        }
        other => Err(AndroidDriverError::InvalidPath(format!(
            "unsupported ZIP compression method {other} for APK `{entry_name}` (expected Store=0 or Deflate=8)"
        ))),
    }
}

// ============================================================================
// Compact, bounded RFC 1951 (raw DEFLATE) decompressor — zero unsafe, zero deps
// ============================================================================

struct BitReader<'a> {
    data: &'a [u8],
    byte_pos: usize,
    bit_buf: u32,
    bit_count: u8,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte_pos: 0,
            bit_buf: 0,
            bit_count: 0,
        }
    }

    fn read_bits(&mut self, n: u8) -> Result<u32> {
        while self.bit_count < n {
            if self.byte_pos >= self.data.len() {
                return Err(AndroidDriverError::InvalidPath(
                    "unexpected end of DEFLATE stream in APK AndroidManifest.xml".to_string(),
                ));
            }
            self.bit_buf |= u32::from(self.data[self.byte_pos]) << self.bit_count;
            self.byte_pos += 1;
            self.bit_count += 8;
        }
        let mask = if n == 32 { u32::MAX } else { (1u32 << n) - 1 };
        let val = self.bit_buf & mask;
        self.bit_buf >>= n;
        self.bit_count -= n;
        Ok(val)
    }

    fn align_to_byte(&mut self) {
        let discard = self.bit_count & 7;
        self.bit_buf >>= discard;
        self.bit_count -= discard;
    }

    fn read_aligned_u16(&mut self) -> Result<u16> {
        let lo = self.read_bits(8)?;
        let hi = self.read_bits(8)?;
        Ok((lo | (hi << 8)) as u16)
    }

    fn read_aligned_bytes(&mut self, len: usize, out: &mut Vec<u8>, max_out: usize) -> Result<()> {
        if out.len().saturating_add(len) > max_out {
            return Err(AndroidDriverError::InvalidPath(format!(
                "APK AndroidManifest.xml decompressed output exceeds {max_out}-byte limit"
            )));
        }
        // Drain any whole bytes already buffered in `bit_buf`.
        let mut remaining = len;
        while remaining > 0 && self.bit_count >= 8 {
            out.push((self.bit_buf & 0xff) as u8);
            self.bit_buf >>= 8;
            self.bit_count -= 8;
            remaining -= 1;
        }
        if self.byte_pos.saturating_add(remaining) > self.data.len() {
            return Err(AndroidDriverError::InvalidPath(
                "truncated stored DEFLATE block in APK AndroidManifest.xml".to_string(),
            ));
        }
        out.extend_from_slice(&self.data[self.byte_pos..self.byte_pos + remaining]);
        self.byte_pos += remaining;
        Ok(())
    }
}

const MAX_HUFFMAN_BITS: usize = 15;

struct HuffmanTable {
    counts: [u16; MAX_HUFFMAN_BITS + 1],
    symbols: Vec<u16>,
}

impl HuffmanTable {
    fn from_lengths(lengths: &[u8]) -> Result<Self> {
        let mut counts = [0u16; MAX_HUFFMAN_BITS + 1];
        for &len in lengths {
            let l = usize::from(len);
            if l > MAX_HUFFMAN_BITS {
                return Err(AndroidDriverError::InvalidPath(
                    "invalid DEFLATE Huffman code length > 15".to_string(),
                ));
            }
            if l > 0 {
                counts[l] = counts[l].saturating_add(1);
            }
        }

        // Validate code tree is not oversubscribed.
        let mut left: i32 = 1;
        for &c in &counts[1..] {
            left = (left << 1) - i32::from(c);
            if left < 0 {
                return Err(AndroidDriverError::InvalidPath(
                    "oversubscribed DEFLATE Huffman tree".to_string(),
                ));
            }
        }

        let mut offsets = [0usize; MAX_HUFFMAN_BITS + 1];
        let mut total = 0usize;
        for len in 1..=MAX_HUFFMAN_BITS {
            offsets[len] = total;
            total += usize::from(counts[len]);
        }
        let mut symbols = vec![0u16; total];
        for (sym, &len) in lengths.iter().enumerate() {
            let l = usize::from(len);
            if l > 0 {
                symbols[offsets[l]] = sym as u16;
                offsets[l] += 1;
            }
        }
        Ok(Self { counts, symbols })
    }

    fn decode(&self, bits: &mut BitReader<'_>) -> Result<u16> {
        let mut code: i32 = 0;
        let mut first: i32 = 0;
        let mut index: usize = 0;
        for len in 1..=MAX_HUFFMAN_BITS {
            code |= bits.read_bits(1)? as i32;
            let count = i32::from(self.counts[len]);
            if code - count < first {
                return self
                    .symbols
                    .get(index + (code - first) as usize)
                    .copied()
                    .ok_or_else(|| {
                        AndroidDriverError::InvalidPath(
                            "invalid DEFLATE Huffman symbol index".to_string(),
                        )
                    });
            }
            index += count as usize;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err(AndroidDriverError::InvalidPath(
            "invalid DEFLATE Huffman code in APK AndroidManifest.xml".to_string(),
        ))
    }
}

const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LEN_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
const CODE_LENGTH_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

fn fixed_huffman_tables() -> Result<(HuffmanTable, HuffmanTable)> {
    let mut lit_lengths = [0u8; 288];
    for item in &mut lit_lengths[..=143] {
        *item = 8;
    }
    for item in &mut lit_lengths[144..=255] {
        *item = 9;
    }
    for item in &mut lit_lengths[256..=279] {
        *item = 7;
    }
    for item in &mut lit_lengths[280..=287] {
        *item = 8;
    }
    let dist_lengths = [5u8; 32];
    Ok((
        HuffmanTable::from_lengths(&lit_lengths)?,
        HuffmanTable::from_lengths(&dist_lengths)?,
    ))
}

fn read_dynamic_huffman_tables(bits: &mut BitReader<'_>) -> Result<(HuffmanTable, HuffmanTable)> {
    let hlit = bits.read_bits(5)? as usize + 257;
    let hdist = bits.read_bits(5)? as usize + 1;
    let hclen = bits.read_bits(4)? as usize + 4;
    if hlit > 286 || hdist > 30 {
        return Err(AndroidDriverError::InvalidPath(
            "invalid DEFLATE dynamic Huffman header counts".to_string(),
        ));
    }

    let mut code_len_lengths = [0u8; 19];
    for &idx in &CODE_LENGTH_ORDER[..hclen] {
        code_len_lengths[idx] = bits.read_bits(3)? as u8;
    }
    let code_len_table = HuffmanTable::from_lengths(&code_len_lengths)?;

    let total_codes = hlit + hdist;
    let mut lengths = Vec::with_capacity(total_codes);
    while lengths.len() < total_codes {
        let sym = code_len_table.decode(bits)?;
        match sym {
            0..=15 => lengths.push(sym as u8),
            16 => {
                let Some(&prev) = lengths.last() else {
                    return Err(AndroidDriverError::InvalidPath(
                        "invalid DEFLATE code-length repeat symbol 16 at start".to_string(),
                    ));
                };
                let repeat = bits.read_bits(2)? as usize + 3;
                if lengths.len() + repeat > total_codes {
                    return Err(AndroidDriverError::InvalidPath(
                        "DEFLATE code-length repeat exceeds declared symbol count".to_string(),
                    ));
                }
                lengths.resize(lengths.len() + repeat, prev);
            }
            17 => {
                let repeat = bits.read_bits(3)? as usize + 3;
                if lengths.len() + repeat > total_codes {
                    return Err(AndroidDriverError::InvalidPath(
                        "DEFLATE zero-repeat symbol 17 exceeds declared symbol count".to_string(),
                    ));
                }
                lengths.resize(lengths.len() + repeat, 0);
            }
            18 => {
                let repeat = bits.read_bits(7)? as usize + 11;
                if lengths.len() + repeat > total_codes {
                    return Err(AndroidDriverError::InvalidPath(
                        "DEFLATE zero-repeat symbol 18 exceeds declared symbol count".to_string(),
                    ));
                }
                lengths.resize(lengths.len() + repeat, 0);
            }
            _ => {
                return Err(AndroidDriverError::InvalidPath(
                    "invalid DEFLATE code-length symbol".to_string(),
                ));
            }
        }
    }

    let lit_table = HuffmanTable::from_lengths(&lengths[..hlit])?;
    let dist_table = HuffmanTable::from_lengths(&lengths[hlit..])?;
    Ok((lit_table, dist_table))
}

fn inflate_raw(input: &[u8], max_output: usize) -> Result<Vec<u8>> {
    let mut bits = BitReader::new(input);
    let mut out = Vec::new();

    loop {
        let bfinal = bits.read_bits(1)?;
        let btype = bits.read_bits(2)?;

        match btype {
            0 => {
                bits.align_to_byte();
                let len = bits.read_aligned_u16()?;
                let nlen = bits.read_aligned_u16()?;
                if len != !nlen {
                    return Err(AndroidDriverError::InvalidPath(
                        "corrupt stored DEFLATE block length checksum".to_string(),
                    ));
                }
                bits.read_aligned_bytes(usize::from(len), &mut out, max_output)?;
            }
            1 | 2 => {
                let (lit_table, dist_table) = if btype == 1 {
                    fixed_huffman_tables()?
                } else {
                    read_dynamic_huffman_tables(&mut bits)?
                };
                loop {
                    let sym = lit_table.decode(&mut bits)?;
                    if sym < 256 {
                        if out.len() >= max_output {
                            return Err(AndroidDriverError::InvalidPath(format!(
                                "APK AndroidManifest.xml decompressed output exceeds {max_output}-byte limit"
                            )));
                        }
                        out.push(sym as u8);
                    } else if sym == 256 {
                        break;
                    } else {
                        let len_idx = usize::from(sym - 257);
                        if len_idx >= LEN_BASE.len() {
                            return Err(AndroidDriverError::InvalidPath(format!(
                                "invalid DEFLATE length symbol {sym}"
                            )));
                        }
                        let length = usize::from(LEN_BASE[len_idx])
                            + bits.read_bits(LEN_EXTRA[len_idx])? as usize;

                        let dist_sym = usize::from(dist_table.decode(&mut bits)?);
                        if dist_sym >= DIST_BASE.len() {
                            return Err(AndroidDriverError::InvalidPath(format!(
                                "invalid DEFLATE distance symbol {dist_sym}"
                            )));
                        }
                        let dist = usize::from(DIST_BASE[dist_sym])
                            + bits.read_bits(DIST_EXTRA[dist_sym])? as usize;
                        if dist == 0 || dist > out.len() {
                            return Err(AndroidDriverError::InvalidPath(
                                "DEFLATE back-reference distance out of bounds".to_string(),
                            ));
                        }
                        if out.len().saturating_add(length) > max_output {
                            return Err(AndroidDriverError::InvalidPath(format!(
                                "APK AndroidManifest.xml decompressed output exceeds {max_output}-byte limit"
                            )));
                        }
                        for _ in 0..length {
                            let b = out[out.len() - dist];
                            out.push(b);
                        }
                    }
                }
            }
            _ => {
                return Err(AndroidDriverError::InvalidPath(
                    "reserved DEFLATE block type 3 in APK AndroidManifest.xml".to_string(),
                ));
            }
        }

        if bfinal != 0 {
            break;
        }
    }

    Ok(out)
}

// ============================================================================
// Android Binary XML (AXML) parser for `<manifest package="...">`
// ============================================================================

fn parse_axml_manifest_package(axml: &[u8]) -> Result<String> {
    if axml.len() < 8 {
        return Err(AndroidDriverError::InvalidPath(
            "APK AndroidManifest.xml is too short to be valid Android Binary XML".to_string(),
        ));
    }
    let root_type = read_u16_le(axml, 0).unwrap_or(0);
    let root_header_size = usize::from(read_u16_le(axml, 2).unwrap_or(0));
    let root_chunk_size = read_u32_le(axml, 4).unwrap_or(0) as usize;
    if root_type != RES_XML_TYPE || root_header_size < 8 || root_chunk_size < root_header_size {
        return Err(AndroidDriverError::InvalidPath(
            "APK AndroidManifest.xml is not valid Android Binary XML (RES_XML_TYPE)".to_string(),
        ));
    }
    let limit = root_chunk_size.min(axml.len());
    let mut offset = root_header_size;
    let mut string_pool: Option<Vec<String>> = None;

    while offset + 8 <= limit {
        let chunk_type = read_u16_le(axml, offset).unwrap_or(0);
        let header_size = usize::from(read_u16_le(axml, offset + 2).unwrap_or(0));
        let chunk_size = read_u32_le(axml, offset + 4).unwrap_or(0) as usize;
        if header_size < 8 || chunk_size < header_size || offset + chunk_size > limit {
            return Err(AndroidDriverError::InvalidPath(
                "corrupt chunk header in APK AndroidManifest.xml".to_string(),
            ));
        }

        let chunk = &axml[offset..offset + chunk_size];
        match chunk_type {
            RES_STRING_POOL_TYPE if string_pool.is_none() => {
                string_pool = Some(parse_string_pool(chunk, header_size)?);
            }
            RES_XML_START_ELEMENT_TYPE => {
                let Some(ref pool) = string_pool else {
                    return Err(AndroidDriverError::InvalidPath(
                        "APK AndroidManifest.xml element appeared before string pool".to_string(),
                    ));
                };
                if let Some(pkg) = parse_manifest_start_element(chunk, header_size, pool)? {
                    return validate_package_id(&pkg);
                }
            }
            _ => {}
        }

        offset += chunk_size;
    }

    Err(AndroidDriverError::InvalidPath(
        "APK AndroidManifest.xml is missing `<manifest package=\"...\">`".to_string(),
    ))
}

fn parse_string_pool(chunk: &[u8], header_size: usize) -> Result<Vec<String>> {
    if header_size < 28 || chunk.len() < header_size {
        return Err(AndroidDriverError::InvalidPath(
            "truncated AXML string pool header".to_string(),
        ));
    }
    let string_count = read_u32_le(chunk, 8).unwrap_or(0) as usize;
    let flags = read_u32_le(chunk, 16).unwrap_or(0);
    let strings_start = read_u32_le(chunk, 20).unwrap_or(0) as usize;
    let is_utf8 = (flags & STRING_POOL_UTF8_FLAG) != 0;

    let offsets_end = header_size.saturating_add(string_count.saturating_mul(4));
    if offsets_end > chunk.len() || strings_start > chunk.len() {
        return Err(AndroidDriverError::InvalidPath(
            "AXML string pool offsets exceed chunk bounds".to_string(),
        ));
    }

    let mut out = Vec::with_capacity(string_count);
    for i in 0..string_count {
        let rel = read_u32_le(chunk, header_size + i * 4).unwrap_or(0) as usize;
        let pos = strings_start.saturating_add(rel);
        if pos >= chunk.len() {
            return Err(AndroidDriverError::InvalidPath(
                "AXML string pool entry offset out of bounds".to_string(),
            ));
        }
        let s = if is_utf8 {
            decode_axml_utf8_string(chunk, pos)?
        } else {
            decode_axml_utf16_string(chunk, pos)?
        };
        out.push(s);
    }
    Ok(out)
}

fn decode_axml_utf8_len(chunk: &[u8], pos: &mut usize) -> Result<usize> {
    let first = *chunk.get(*pos).ok_or_else(|| {
        AndroidDriverError::InvalidPath("truncated AXML UTF-8 string length".to_string())
    })?;
    *pos += 1;
    if (first & 0x80) != 0 {
        let second = *chunk.get(*pos).ok_or_else(|| {
            AndroidDriverError::InvalidPath("truncated 2-byte AXML UTF-8 string length".to_string())
        })?;
        *pos += 1;
        Ok((usize::from(first & 0x7f) << 8) | usize::from(second))
    } else {
        Ok(usize::from(first))
    }
}

fn decode_axml_utf8_string(chunk: &[u8], start: usize) -> Result<String> {
    let mut pos = start;
    // First varint is UTF-16 character count; second varint is UTF-8 byte count.
    let _utf16_len = decode_axml_utf8_len(chunk, &mut pos)?;
    let byte_len = decode_axml_utf8_len(chunk, &mut pos)?;
    let end = pos.saturating_add(byte_len);
    let raw = chunk.get(pos..end).ok_or_else(|| {
        AndroidDriverError::InvalidPath("AXML UTF-8 string bytes out of bounds".to_string())
    })?;
    Ok(String::from_utf8_lossy(raw).into_owned())
}

fn decode_axml_utf16_string(chunk: &[u8], start: usize) -> Result<String> {
    let mut pos = start;
    let first = read_u16_le(chunk, pos).ok_or_else(|| {
        AndroidDriverError::InvalidPath("truncated AXML UTF-16 string length".to_string())
    })?;
    pos += 2;
    let char_len = if (first & 0x8000) != 0 {
        let second = read_u16_le(chunk, pos).ok_or_else(|| {
            AndroidDriverError::InvalidPath(
                "truncated 4-byte AXML UTF-16 string length".to_string(),
            )
        })?;
        pos += 2;
        ((usize::from(first & 0x7fff)) << 16) | usize::from(second)
    } else {
        usize::from(first)
    };

    let byte_len = char_len.saturating_mul(2);
    if pos.saturating_add(byte_len) > chunk.len() {
        return Err(AndroidDriverError::InvalidPath(
            "AXML UTF-16 string bytes out of bounds".to_string(),
        ));
    }
    let mut units = Vec::with_capacity(char_len);
    for i in 0..char_len {
        units.push(read_u16_le(chunk, pos + i * 2).unwrap_or(0));
    }
    Ok(String::from_utf16_lossy(&units))
}

fn parse_manifest_start_element(
    chunk: &[u8],
    header_size: usize,
    pool: &[String],
) -> Result<Option<String>> {
    if header_size + 20 > chunk.len() {
        return Err(AndroidDriverError::InvalidPath(
            "truncated AXML start-element chunk".to_string(),
        ));
    }
    let ns_idx = read_u32_le(chunk, header_size).unwrap_or(AXML_NO_INDEX);
    let name_idx = read_u32_le(chunk, header_size + 4).unwrap_or(AXML_NO_INDEX);
    let attr_start = usize::from(read_u16_le(chunk, header_size + 8).unwrap_or(0));
    let attr_size = usize::from(read_u16_le(chunk, header_size + 10).unwrap_or(0));
    let attr_count = usize::from(read_u16_le(chunk, header_size + 12).unwrap_or(0));

    let elem_name = pool.get(name_idx as usize).map(String::as_str);
    let elem_ns_empty =
        ns_idx == AXML_NO_INDEX || pool.get(ns_idx as usize).is_some_and(|s| s.is_empty());
    if elem_name != Some("manifest") || !elem_ns_empty {
        return Ok(None);
    }

    if attr_count > 0 && attr_size < 20 {
        return Err(AndroidDriverError::InvalidPath(
            "invalid AXML attribute stride < 20 bytes".to_string(),
        ));
    }
    let attrs_base = header_size.saturating_add(attr_start);
    for i in 0..attr_count {
        let a_off = attrs_base.saturating_add(i.saturating_mul(attr_size));
        if a_off + 20 > chunk.len() {
            return Err(AndroidDriverError::InvalidPath(
                "truncated AXML attribute table on <manifest>".to_string(),
            ));
        }
        let attr_ns = read_u32_le(chunk, a_off).unwrap_or(AXML_NO_INDEX);
        let attr_name_idx = read_u32_le(chunk, a_off + 4).unwrap_or(AXML_NO_INDEX);
        let raw_value_idx = read_u32_le(chunk, a_off + 8).unwrap_or(AXML_NO_INDEX);
        let data_type = chunk[a_off + 15];
        let data = read_u32_le(chunk, a_off + 16).unwrap_or(AXML_NO_INDEX);

        let attr_ns_empty =
            attr_ns == AXML_NO_INDEX || pool.get(attr_ns as usize).is_some_and(|s| s.is_empty());
        let attr_name = pool.get(attr_name_idx as usize).map(String::as_str);
        if attr_ns_empty && attr_name == Some("package") {
            let pkg_str = if raw_value_idx != AXML_NO_INDEX {
                pool.get(raw_value_idx as usize).cloned()
            } else if data_type == AXML_TYPE_STRING {
                pool.get(data as usize).cloned()
            } else {
                None
            };
            return pkg_str.map(Some).ok_or_else(|| {
                AndroidDriverError::InvalidPath(
                    "APK `<manifest package>` attribute does not reference a valid string"
                        .to_string(),
                )
            });
        }
    }

    Err(AndroidDriverError::InvalidPath(
        "APK `<manifest>` element is missing required `package` attribute".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        AXML_NO_INDEX, AXML_TYPE_STRING, RES_STRING_POOL_TYPE, RES_XML_START_ELEMENT_TYPE,
        RES_XML_TYPE, STRING_POOL_UTF8_FLAG, ZIP_CENTRAL_DIR_HEADER_SIG, ZIP_EOCD_SIG,
        ZIP_LOCAL_FILE_HEADER_SIG, ZIP_METHOD_DEFLATE, ZIP_METHOD_STORE, extract_apk_package_id,
        extract_apk_package_id_from_path,
    };
    use crate::embed::PREBUILT_AGENT_APK;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn build_synthetic_axml(package_id: &str, utf8: bool) -> Vec<u8> {
        let strings = ["manifest", "package", package_id];
        let mut string_data = Vec::new();
        let mut offsets = Vec::new();
        for s in strings {
            offsets.push(string_data.len() as u32);
            if utf8 {
                let bytes = s.as_bytes();
                let u16_len = s.encode_utf16().count();
                string_data.push(u16_len as u8);
                string_data.push(bytes.len() as u8);
                string_data.extend_from_slice(bytes);
                string_data.push(0);
            } else {
                let units: Vec<u16> = s.encode_utf16().collect();
                string_data.extend_from_slice(&(units.len() as u16).to_le_bytes());
                for u in units {
                    string_data.extend_from_slice(&u.to_le_bytes());
                }
                string_data.extend_from_slice(&0u16.to_le_bytes());
            }
        }
        while string_data.len() % 4 != 0 {
            string_data.push(0);
        }

        let sp_header_size: u16 = 28;
        let strings_start: u32 = u32::from(sp_header_size) + (strings.len() as u32) * 4;
        let sp_chunk_size: u32 = strings_start + string_data.len() as u32;
        let mut sp_chunk = Vec::new();
        sp_chunk.extend_from_slice(&RES_STRING_POOL_TYPE.to_le_bytes());
        sp_chunk.extend_from_slice(&sp_header_size.to_le_bytes());
        sp_chunk.extend_from_slice(&sp_chunk_size.to_le_bytes());
        sp_chunk.extend_from_slice(&(strings.len() as u32).to_le_bytes());
        sp_chunk.extend_from_slice(&0u32.to_le_bytes()); // styleCount
        sp_chunk
            .extend_from_slice(&(if utf8 { STRING_POOL_UTF8_FLAG } else { 0u32 }).to_le_bytes());
        sp_chunk.extend_from_slice(&strings_start.to_le_bytes());
        sp_chunk.extend_from_slice(&0u32.to_le_bytes()); // stylesStart
        for off in offsets {
            sp_chunk.extend_from_slice(&off.to_le_bytes());
        }
        sp_chunk.extend_from_slice(&string_data);

        // Start element chunk: <manifest package="...">
        let se_header_size: u16 = 16;
        let se_body_size: u32 = 20 + 20; // element fields + 1 attribute
        let se_chunk_size: u32 = u32::from(se_header_size) + se_body_size;
        let mut se_chunk = Vec::new();
        se_chunk.extend_from_slice(&RES_XML_START_ELEMENT_TYPE.to_le_bytes());
        se_chunk.extend_from_slice(&se_header_size.to_le_bytes());
        se_chunk.extend_from_slice(&se_chunk_size.to_le_bytes());
        se_chunk.extend_from_slice(&1u32.to_le_bytes()); // lineNumber
        se_chunk.extend_from_slice(&AXML_NO_INDEX.to_le_bytes()); // comment
        se_chunk.extend_from_slice(&AXML_NO_INDEX.to_le_bytes()); // ns
        se_chunk.extend_from_slice(&0u32.to_le_bytes()); // name = "manifest"
        se_chunk.extend_from_slice(&20u16.to_le_bytes()); // attributeStart
        se_chunk.extend_from_slice(&20u16.to_le_bytes()); // attributeSize
        se_chunk.extend_from_slice(&1u16.to_le_bytes()); // attributeCount
        se_chunk.extend_from_slice(&0u16.to_le_bytes()); // idIndex
        se_chunk.extend_from_slice(&0u16.to_le_bytes()); // classIndex
        se_chunk.extend_from_slice(&0u16.to_le_bytes()); // styleIndex
        // Attribute 0: package="<package_id>"
        se_chunk.extend_from_slice(&AXML_NO_INDEX.to_le_bytes()); // attr ns
        se_chunk.extend_from_slice(&1u32.to_le_bytes()); // attr name = "package"
        se_chunk.extend_from_slice(&2u32.to_le_bytes()); // rawValue = package_id
        se_chunk.extend_from_slice(&8u16.to_le_bytes()); // typedValue size
        se_chunk.push(0); // res0
        se_chunk.push(AXML_TYPE_STRING); // dataType
        se_chunk.extend_from_slice(&2u32.to_le_bytes()); // data = package_id

        let total_size = 8 + sp_chunk.len() + se_chunk.len();
        let mut axml = Vec::with_capacity(total_size);
        axml.extend_from_slice(&RES_XML_TYPE.to_le_bytes());
        axml.extend_from_slice(&8u16.to_le_bytes());
        axml.extend_from_slice(&(total_size as u32).to_le_bytes());
        axml.extend_from_slice(&sp_chunk);
        axml.extend_from_slice(&se_chunk);
        axml
    }

    fn deflate_stored_block(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len() + 5);
        out.push(0x01); // BFINAL=1, BTYPE=00 (stored)
        let len = data.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(data);
        out
    }

    fn build_single_entry_zip(
        name: &str,
        method: u16,
        compressed: &[u8],
        uncompressed_len: u32,
    ) -> Vec<u8> {
        let name_bytes = name.as_bytes();
        let mut zip = Vec::new();
        let local_offset = zip.len() as u32;
        zip.extend_from_slice(&ZIP_LOCAL_FILE_HEADER_SIG.to_le_bytes());
        zip.extend_from_slice(&20u16.to_le_bytes()); // version needed
        zip.extend_from_slice(&0u16.to_le_bytes()); // flags
        zip.extend_from_slice(&method.to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes()); // mod time
        zip.extend_from_slice(&0u16.to_le_bytes()); // mod date
        zip.extend_from_slice(&0u32.to_le_bytes()); // crc32
        zip.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
        zip.extend_from_slice(&uncompressed_len.to_le_bytes());
        zip.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes()); // extra len
        zip.extend_from_slice(name_bytes);
        zip.extend_from_slice(compressed);

        let cd_offset = zip.len() as u32;
        zip.extend_from_slice(&ZIP_CENTRAL_DIR_HEADER_SIG.to_le_bytes());
        zip.extend_from_slice(&20u16.to_le_bytes()); // version made by
        zip.extend_from_slice(&20u16.to_le_bytes()); // version needed
        zip.extend_from_slice(&0u16.to_le_bytes()); // flags
        zip.extend_from_slice(&method.to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes()); // mod time
        zip.extend_from_slice(&0u16.to_le_bytes()); // mod date
        zip.extend_from_slice(&0u32.to_le_bytes()); // crc32
        zip.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
        zip.extend_from_slice(&uncompressed_len.to_le_bytes());
        zip.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes()); // extra len
        zip.extend_from_slice(&0u16.to_le_bytes()); // comment len
        zip.extend_from_slice(&0u16.to_le_bytes()); // disk start
        zip.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        zip.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        zip.extend_from_slice(&local_offset.to_le_bytes());
        zip.extend_from_slice(name_bytes);

        let cd_size = (zip.len() as u32) - cd_offset;
        zip.extend_from_slice(&ZIP_EOCD_SIG.to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes()); // disk
        zip.extend_from_slice(&0u16.to_le_bytes()); // cd start disk
        zip.extend_from_slice(&1u16.to_le_bytes()); // entries on disk
        zip.extend_from_slice(&1u16.to_le_bytes()); // total entries
        zip.extend_from_slice(&cd_size.to_le_bytes());
        zip.extend_from_slice(&cd_offset.to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes()); // comment len
        zip
    }

    #[test]
    fn extracts_package_id_from_real_compiled_prebuilt_agent_apk() {
        let pkg = extract_apk_package_id(PREBUILT_AGENT_APK)
            .expect("real prebuilt deja-android-agent.apk must parse cleanly");
        assert_eq!(pkg, "dev.deja.agent");
    }

    #[test]
    fn extracts_package_id_from_stored_utf16_and_deflated_utf8_apks() {
        // 1. Store (method 0) + UTF-16LE string pool.
        let axml_u16 = build_synthetic_axml("com.example.stored_u16", false);
        let zip_stored = build_single_entry_zip(
            "AndroidManifest.xml",
            ZIP_METHOD_STORE,
            &axml_u16,
            axml_u16.len() as u32,
        );
        assert_eq!(
            extract_apk_package_id(&zip_stored).expect("stored UTF-16 APK"),
            "com.example.stored_u16"
        );

        // 2. Deflate (method 8) + UTF-8 string pool.
        let axml_u8 = build_synthetic_axml("com.example.deflated_u8", true);
        let deflated = deflate_stored_block(&axml_u8);
        let zip_deflated = build_single_entry_zip(
            "AndroidManifest.xml",
            ZIP_METHOD_DEFLATE,
            &deflated,
            axml_u8.len() as u32,
        );
        assert_eq!(
            extract_apk_package_id(&zip_deflated).expect("deflated UTF-8 APK"),
            "com.example.deflated_u8"
        );

        // 3. Path-based extractor reads cleanly from disk and refuses symlinks.
        let dir = std::env::temp_dir().join(format!(
            "deja-apk-extract-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let apk_path = dir.join("app.apk");
        fs::write(&apk_path, &zip_deflated).unwrap();
        assert_eq!(
            extract_apk_package_id_from_path(&apk_path).expect("path extract"),
            "com.example.deflated_u8"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let link = dir.join("link.apk");
            symlink(&apk_path, &link).unwrap();
            assert!(extract_apk_package_id_from_path(&link).is_err());
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_non_apk_missing_manifest_corrupt_axml_and_oversized_manifest() {
        assert!(extract_apk_package_id(b"not a zip file").is_err());
        assert!(extract_apk_package_id(b"PK\x03\x04truncated-fake-apk").is_err());

        // Valid ZIP missing AndroidManifest.xml.
        let no_manifest = build_single_entry_zip("classes.dex", ZIP_METHOD_STORE, b"dex", 3);
        assert!(extract_apk_package_id(&no_manifest).is_err());

        // Valid ZIP with garbage AndroidManifest.xml (not AXML).
        let bad_axml =
            build_single_entry_zip("AndroidManifest.xml", ZIP_METHOD_STORE, b"<manifest/>", 11);
        assert!(extract_apk_package_id(&bad_axml).is_err());

        // Declared manifest > 4 MiB zip-bomb cap fails closed before reading/inflating.
        let bomb = build_single_entry_zip(
            "AndroidManifest.xml",
            ZIP_METHOD_DEFLATE,
            &[0x01, 0x00, 0x00, 0xff, 0xff],
            16 * 1024 * 1024,
        );
        assert!(extract_apk_package_id(&bomb).is_err());
    }
}
