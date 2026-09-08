//! Minimal readers for dependency jars: a ZIP central-directory reader and a
//! minimal Java class-file parser that extracts what the index needs — the
//! class kind, its nested-name container chain, and declared members.
//!
//! Declaration-only by design: descriptors and attributes are skipped, so a
//! class file yields names, not types (member descriptors are a later
//! refinement). Everything here runs on the background scan task.

use std::io::Read;
use std::path::Path;

use tower_lsp::lsp_types::{Position, Range, Url};

use crate::index::{IndexKind, SymbolEntry};

/// Parsed surface of one `.class` file: the pieces the index stores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassInfo {
    /// Internal name, e.g. `com/example/Foo$Bar`.
    pub internal_name: String,
    pub kind: IndexKind,
    pub methods: Vec<String>,
    pub fields: Vec<String>,
}

/// Reads every `.class` entry in the jar at `path` and turns it into index
/// entries attributed to the jar's own file URI, flagged as dependencies.
pub fn entries_from_jar(path: &Path) -> Option<Vec<SymbolEntry>> {
    let data = std::fs::read(path).ok()?;
    let jar_url = Url::from_file_path(path).ok()?;
    let entries = read_zip_entries(&data)?;
    let mut out = Vec::new();
    for (name, data) in entries {
        if !name.ends_with(".class") || name.contains("module-info") {
            continue;
        }
        let Ok(info) = parse_class(&data) else {
            continue;
        };
        out.extend(class_entries(&jar_url, &info));
    }
    Some(out)
}

/// One jar (or class) entry per type plus one per declared member; ranges are
/// zero because jar locations are not openable, and these entries are flagged
/// as dependencies so navigation filters them out. The package comes from the
/// internal name so completions can import the type.
pub fn class_entries(jar_url: &Url, info: &ClassInfo) -> Vec<SymbolEntry> {
    let zero = Range::new(Position::new(0, 0), Position::new(0, 0));
    let simple = info.internal_name.rsplit('/').next().unwrap_or("");
    let package = info
        .internal_name
        .rsplit_once('/')
        .map(|(directory, _)| directory.replace('/', "."));
    let mut container: Vec<String> = simple
        .split('$')
        .filter(|segment| !segment.is_empty())
        .map(str::to_string)
        .collect();
    let name = container.pop().unwrap_or_default();
    if name.is_empty() || name.chars().all(|c| c.is_ascii_digit()) {
        return Vec::new();
    }

    let mut out = vec![SymbolEntry {
        uri: jar_url.clone(),
        name: name.clone(),
        kind: info.kind,
        package: package.clone(),
        container: container.clone(),
        full_range: zero,
        selection_range: zero,
        dependency: true,
    }];
    for method in &info.methods {
        out.push(SymbolEntry {
            uri: jar_url.clone(),
            name: method.clone(),
            kind: IndexKind::Method,
            package: package.clone(),
            container: {
                let mut c = container.clone();
                c.push(name.clone());
                c
            },
            full_range: zero,
            selection_range: zero,
            dependency: true,
        });
    }
    for field in &info.fields {
        out.push(SymbolEntry {
            uri: jar_url.clone(),
            name: field.clone(),
            kind: IndexKind::Field,
            package: package.clone(),
            container: {
                let mut c = container.clone();
                c.push(name.clone());
                c
            },
            full_range: zero,
            selection_range: zero,
            dependency: true,
        });
    }
    out
}

/// Parses the minimal surface of a Java class file (JVM spec: constant pool,
/// access flags, this/super class, field and method tables).
pub fn parse_class(data: &[u8]) -> Result<ClassInfo, String> {
    let mut reader = Reader { data, pos: 0 };
    if reader.u4()? != 0xCAFEBABE {
        return Err("not a class file".into());
    }
    reader.u2()?; // minor
    reader.u2()?; // major

    // Constant pool; only Class (7) and Utf8 (1) entries are kept.
    let cp_count = reader.u2()?;
    let mut class_names: Vec<Option<usize>> = vec![None; cp_count as usize];
    let mut utf8: Vec<String> = Vec::new();
    let mut utf8_slots: Vec<usize> = Vec::new();
    let mut index = 1usize;
    while index < cp_count as usize {
        match reader.u1()? {
            1 => {
                let len = reader.u2()? as usize;
                let bytes = reader.bytes(len)?;
                utf8.push(String::from_utf8_lossy(bytes).into_owned());
                utf8_slots.push(index);
                index += 1;
            }
            3 | 4 => {
                reader.u4()?;
                index += 1;
            }
            5 | 6 => {
                reader.u4()?;
                reader.u4()?;
                index += 2; // double-width entries
            }
            7 => {
                let name_index = reader.u2()? as usize;
                class_names[index] = Some(name_index);
                index += 1;
            }
            8 | 13 | 16 => {
                reader.u2()?;
                index += 1;
            }
            9 | 10 | 11 | 12 | 17 | 18 => {
                reader.u2()?;
                reader.u2()?;
                index += 1;
            }
            15 => {
                reader.u1()?;
                reader.u2()?;
                index += 1;
            }
            19 | 20 => {
                reader.u2()?;
                index += 1;
            }
            other => return Err(format!("unknown constant pool tag {other}")),
        }
    }
    let utf8_at = |slot: usize| -> Option<&str> {
        let position = utf8_slots.iter().position(|&i| i == slot)?;
        utf8.get(position).map(String::as_str)
    };
    let class_name = |slot: usize| -> Option<String> {
        class_names
            .get(slot)
            .copied()
            .flatten()
            .and_then(|name_index| utf8_at(name_index).map(str::to_string))
    };

    let flags = reader.u2()?;
    let this_class = reader.u2()? as usize;
    let super_class = reader.u2()? as usize;
    let Some(internal_name) = class_name(this_class) else {
        return Err("unresolvable this_class".into());
    };
    let interfaces = reader.u2()? as usize;
    reader.bytes(interfaces * 2)?;

    let fields = {
        let count = reader.u2()?;
        read_members(&mut reader, count, &utf8_at)?
    };
    let methods = {
        let count = reader.u2()?;
        read_members(&mut reader, count, &utf8_at)?
    };
    let attributes = reader.u2()?;
    for _ in 0..attributes {
        reader.u2()?;
        let len = reader.u4()? as usize;
        reader.bytes(len)?;
    }

    let kind = if flags & 0x0200 != 0 {
        IndexKind::Interface // includes annotations
    } else if super_class != 0 && class_name(super_class).as_deref() == Some("java/lang/Record") {
        IndexKind::Record
    } else if flags & 0x4000 != 0 {
        IndexKind::Enum
    } else {
        IndexKind::Class
    };

    Ok(ClassInfo {
        internal_name,
        kind,
        methods,
        fields,
    })
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

/// Reads one member table (fields or methods): `flags/name/descriptor` plus
/// skipped attributes per entry; keeps only source-visible member names.
fn read_members<'a>(
    reader: &mut Reader<'a>,
    count: u16,
    utf8_at: &dyn Fn(usize) -> Option<&'a str>,
) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    for _ in 0..count {
        let member_flags = reader.u2()?;
        let name_index = reader.u2()? as usize;
        reader.u2()?; // descriptor
        let attributes = reader.u2()?;
        for _ in 0..attributes {
            reader.u2()?; // attribute name
            let len = reader.u4()? as usize;
            reader.bytes(len)?;
        }
        let Some(name) = utf8_at(name_index) else {
            continue;
        };
        // <init>/<clinit> are not source-visible names; `$` marks synthetic
        // members like `this$0` and accessors.
        if name.starts_with('<') || name.contains('$') {
            continue;
        }
        if member_flags & 0x0002 != 0 {
            continue; // ACC_PRIVATE: not visible to callers
        }
        names.push(name.to_string());
    }
    Ok(names)
}

impl Reader<'_> {
    fn bytes(&mut self, len: usize) -> Result<&[u8], String> {
        if self.pos + len > self.data.len() {
            return Err("truncated class file".into());
        }
        let out = &self.data[self.pos..self.pos + len];
        self.pos += len;
        Ok(out)
    }
    fn u1(&mut self) -> Result<u8, String> {
        Ok(self.bytes(1)?[0])
    }
    fn u2(&mut self) -> Result<u16, String> {
        let b = self.bytes(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
    fn u4(&mut self) -> Result<u32, String> {
        let b = self.bytes(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
}

/// One ZIP entry: archive-relative name and its decompressed content.
type ZipEntry = (String, Vec<u8>);

/// Reads (and decompresses) every central-directory entry of an in-memory ZIP
/// whose name passes `filter` — jmods bundle native libraries that must never
/// be decompressed when only class files are wanted.
pub fn read_zip_entries_filtered(
    data: &[u8],
    filter: impl Fn(&str) -> bool,
) -> Option<Vec<ZipEntry>> {
    let mut out = Vec::new();
    for_each_zip_entry(data, |name, content| {
        if filter(&name) {
            out.push((name, content));
        }
    })?;
    Some(out)
}

/// Streaming variant: hands each matching entry to `callback` one at a time,
/// so large archives (src.zip ≈ 25 MB of sources) never materialize fully in
/// memory. Returns `None` when no central directory is found.
pub fn for_each_zip_entry(data: &[u8], mut callback: impl FnMut(String, Vec<u8>)) -> Option<()> {
    let eocd = find_eocd(data)?;
    let entry_count = u16_le(data, eocd + 10)? as usize;
    let mut offset = u32_le(data, eocd + 16)? as usize;

    for _ in 0..entry_count {
        if data.get(offset..offset + 4)? != [0x50, 0x4b, 0x01, 0x02] {
            break; // central directory over; tolerate trailing junk
        }
        let method = u16_le(data, offset + 10)?;
        let compressed_size = u32_le(data, offset + 20)? as usize;
        let name_len = u16_le(data, offset + 28)? as usize;
        let extra_len = u16_le(data, offset + 30)? as usize;
        let comment_len = u16_le(data, offset + 32)? as usize;
        let local_offset = u32_le(data, offset + 42)? as usize;
        let name =
            String::from_utf8_lossy(data.get(offset + 46..offset + 46 + name_len)?).into_owned();

        if let Some(content) = read_entry_data(data, local_offset, method, compressed_size, &name) {
            callback(name, content);
        }

        offset += 46 + name_len + extra_len + comment_len;
    }
    Some(())
}

/// Reads (and decompresses) every central-directory entry of an in-memory ZIP.
pub fn read_zip_entries(data: &[u8]) -> Option<Vec<ZipEntry>> {
    read_zip_entries_filtered(data, |_| true)
}

fn read_entry_data(
    data: &[u8],
    local_offset: usize,
    method: u16,
    compressed_size: usize,
    name: &str,
) -> Option<Vec<u8>> {
    if data.get(local_offset..local_offset + 4)? != [0x50, 0x4b, 0x03, 0x04] {
        return None;
    }
    let name_len = u16_le(data, local_offset + 26)? as usize;
    let extra_len = u16_le(data, local_offset + 28)? as usize;
    let start = local_offset + 30 + name_len + extra_len;
    let compressed = data.get(start..start + compressed_size)?;
    match method {
        0 => Some(compressed.to_vec()),
        8 => {
            let mut out = Vec::new();
            flate2::read::DeflateDecoder::new(compressed)
                .read_to_end(&mut out)
                .ok()?;
            Some(out)
        }
        other => {
            tracing::warn!(entry = name, method = other, "unsupported jar compression");
            None
        }
    }
}

/// Scans backwards for the end-of-central-directory record (comment may
/// follow it, so the signature is located from the archive's tail).
fn find_eocd(data: &[u8]) -> Option<usize> {
    let scan = data.len().saturating_sub(66_000);
    let mut i = data.len().checked_sub(22)?;
    while i >= scan {
        if data[i..i + 4] == [0x50, 0x4b, 0x05, 0x06] {
            return Some(i);
        }
        i -= 1;
    }
    None
}

fn u16_le(data: &[u8], offset: usize) -> Option<u16> {
    let b = data.get(offset..offset + 2)?;
    Some(u16::from_le_bytes([b[0], b[1]]))
}

fn u32_le(data: &[u8], offset: usize) -> Option<u32> {
    let b = data.get(offset..offset + 4)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds class-file bytes with a generated constant pool: one Utf8 +
    /// Class pair for the class itself, one for the superclass, and one Utf8
    /// per member name. A member name prefixed with `!` is written private.
    fn class_bytes(
        internal: &str,
        flags: u16,
        super_name: Option<&str>,
        fields: &[&str],
        methods: &[&str],
    ) -> Vec<u8> {
        fn member_spec(spec: &str, pool: &mut Vec<(u8, Vec<Vec<u8>>)>) -> (u16, u16) {
            let (name, member_flags) = match spec.strip_prefix('!') {
                Some(name) => (name, 0x0002u16),
                None => (spec, 0x0001u16),
            };
            pool.push((
                1,
                vec![
                    (name.len() as u16).to_be_bytes().to_vec(),
                    name.as_bytes().to_vec(),
                ],
            ));
            (pool.len() as u16, member_flags)
        }
        let mut pool: Vec<(u8, Vec<Vec<u8>>)> = Vec::new(); // (tag, operands)
        let utf8_slot = |text: &str, pool: &mut Vec<(u8, Vec<Vec<u8>>)>| -> u16 {
            pool.push((
                1,
                vec![
                    (text.len() as u16).to_be_bytes().to_vec(),
                    text.as_bytes().to_vec(),
                ],
            ));
            pool.len() as u16
        };
        let name_slot = utf8_slot(internal, &mut pool);
        pool.push((7, vec![name_slot.to_be_bytes().to_vec()]));
        let this_class = pool.len() as u16;
        let super_class = super_name.map_or(0, |super_name| {
            let slot = utf8_slot(super_name, &mut pool);
            pool.push((7, vec![slot.to_be_bytes().to_vec()]));
            pool.len() as u16
        });
        let field_specs: Vec<(u16, u16)> = fields
            .iter()
            .map(|spec| member_spec(spec, &mut pool))
            .collect();
        let method_specs: Vec<(u16, u16)> = methods
            .iter()
            .map(|spec| member_spec(spec, &mut pool))
            .collect();

        let mut out = Vec::new();
        out.extend_from_slice(&0xCAFEBABEu32.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes()); // minor
        out.extend_from_slice(&(52u16).to_be_bytes()); // major
        out.extend_from_slice(&((pool.len() + 1) as u16).to_be_bytes());
        for (tag, operands) in &pool {
            out.push(*tag);
            for operand in operands {
                out.extend_from_slice(operand);
            }
        }
        out.extend_from_slice(&flags.to_be_bytes());
        out.extend_from_slice(&this_class.to_be_bytes());
        out.extend_from_slice(&super_class.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes()); // interfaces
        let member_table = |members: &[(u16, u16)], out: &mut Vec<u8>| {
            out.extend_from_slice(&(members.len() as u16).to_be_bytes());
            for (slot, flags) in members {
                out.extend_from_slice(&flags.to_be_bytes());
                out.extend_from_slice(&slot.to_be_bytes()); // name
                out.extend_from_slice(&0u16.to_be_bytes()); // descriptor index (0: unused here)
                out.extend_from_slice(&0u16.to_be_bytes()); // no attributes
            }
        };
        member_table(&field_specs, &mut out);
        member_table(&method_specs, &mut out);
        out.extend_from_slice(&0u16.to_be_bytes()); // class attributes
        out
    }

    /// Minimal STORED (uncompressed) zip writer for tests — std has none.
    fn write_stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        fn crc32(data: &[u8]) -> u32 {
            let mut table = [0u32; 256];
            for (i, slot) in table.iter_mut().enumerate() {
                let mut c = i as u32;
                for _ in 0..8 {
                    c = if c & 1 != 0 {
                        0xEDB88320 ^ (c >> 1)
                    } else {
                        c >> 1
                    };
                }
                *slot = c;
            }
            let mut crc = 0xFFFF_FFFFu32;
            for &byte in data {
                crc = table[((crc ^ byte as u32) & 0xFF) as usize] ^ (crc >> 8);
            }
            crc ^ 0xFFFF_FFFF
        }

        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data) in entries {
            let local_offset = out.len() as u32;
            let crc = crc32(data);
            out.extend_from_slice(&[0x50, 0x4b, 0x03, 0x04]);
            out.extend_from_slice(&20u16.to_le_bytes()); // version needed
            out.extend_from_slice(&0u16.to_le_bytes()); // flags
            out.extend_from_slice(&0u16.to_le_bytes()); // stored
            out.extend_from_slice(&0u16.to_le_bytes()); // time
            out.extend_from_slice(&0u16.to_le_bytes()); // date
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // extra
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);

            central.extend_from_slice(&[0x50, 0x4b, 0x01, 0x02]);
            central.extend_from_slice(&20u16.to_le_bytes()); // version made by
            central.extend_from_slice(&20u16.to_le_bytes()); // version needed
            central.extend_from_slice(&0u16.to_le_bytes()); // flags
            central.extend_from_slice(&0u16.to_le_bytes()); // stored
            central.extend_from_slice(&0u16.to_le_bytes()); // time
            central.extend_from_slice(&0u16.to_le_bytes()); // date
            central.extend_from_slice(&crc.to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes()); // extra
            central.extend_from_slice(&0u16.to_le_bytes()); // comment
            central.extend_from_slice(&0u16.to_le_bytes()); // disk
            central.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
            central.extend_from_slice(&0u32.to_le_bytes()); // external attrs
            central.extend_from_slice(&local_offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let central_offset = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&[0x50, 0x4b, 0x05, 0x06]);
        out.extend_from_slice(&0u16.to_le_bytes()); // disk
        out.extend_from_slice(&0u16.to_le_bytes()); // start disk
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&central_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // comment
        out
    }

    #[test]
    fn parses_a_plain_class_with_members() {
        let bytes = class_bytes(
            "demo/Foo",
            0x0021,
            Some("java/lang/Object"),
            &["size", "name"],
            &["getSize", "setName"],
        );
        let info = parse_class(&bytes).unwrap();
        assert_eq!(info.internal_name, "demo/Foo");
        assert_eq!(info.kind, IndexKind::Class);
        assert_eq!(info.fields, vec!["size", "name"]);
        assert_eq!(info.methods, vec!["getSize", "setName"]);
    }

    #[test]
    fn distinguishes_interface_enum_and_record() {
        let interface = parse_class(&class_bytes(
            "demo/Runnable",
            0x0601,
            Some("java/lang/Object"),
            &[],
            &[],
        ))
        .unwrap();
        assert_eq!(interface.kind, IndexKind::Interface);

        let enumeration = parse_class(&class_bytes(
            "demo/Color",
            0x4021,
            Some("java/lang/Enum"),
            &[],
            &[],
        ))
        .unwrap();
        assert_eq!(enumeration.kind, IndexKind::Enum);

        let record = parse_class(&class_bytes(
            "demo/Point",
            0x0031,
            Some("java/lang/Record"),
            &[],
            &[],
        ))
        .unwrap();
        assert_eq!(record.kind, IndexKind::Record);
    }

    #[test]
    fn skips_constructors_synthetic_and_private_members() {
        let bytes = class_bytes(
            "demo/Foo",
            0x0021,
            Some("java/lang/Object"),
            &["<init>", "!this$0", "size", "!secret"],
            &["<clinit>", "!access$100", "getSize"],
        );
        let info = parse_class(&bytes).unwrap();
        assert_eq!(info.fields, vec!["size"]);
        assert_eq!(info.methods, vec!["getSize"]);
    }

    #[test]
    fn rejects_non_class_bytes() {
        assert!(parse_class(b"PK\x03\x04 not a class").is_err());
        assert!(parse_class(&[]).is_err());
    }

    #[test]
    fn jar_entries_carry_the_class_and_its_members_as_dependencies() {
        let bytes = class_bytes(
            "com/example/lib/Lib",
            0x0021,
            Some("java/lang/Object"),
            &["name"],
            &["getName"],
        );
        let jar = write_stored_zip(&[("com/example/lib/Lib.class", &bytes)]);
        let path = std::env::temp_dir().join(format!(
            "java-lsp-classfile-test-{}-{}.jar",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, &jar).unwrap();

        let entries = entries_from_jar(&path).unwrap();
        let jar_url = Url::from_file_path(&path).unwrap();
        let lib = entries
            .iter()
            .find(|entry| entry.name == "Lib")
            .expect("class entry");
        assert_eq!(lib.kind, IndexKind::Class);
        assert!(lib.dependency);
        assert_eq!(lib.uri, jar_url);
        assert!(lib.container.is_empty());
        assert_eq!(lib.package.as_deref(), Some("com.example.lib"));

        let get_name = entries
            .iter()
            .find(|entry| entry.name == "getName")
            .expect("method entry");
        assert_eq!(get_name.kind, IndexKind::Method);
        assert_eq!(get_name.container, vec!["Lib".to_string()]);
        assert_eq!(get_name.package.as_deref(), Some("com.example.lib"));

        let name = entries
            .iter()
            .find(|entry| entry.name == "name")
            .expect("field entry");
        assert_eq!(name.kind, IndexKind::Field);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn nested_classes_split_into_a_container_chain() {
        let outer = class_bytes("demo/Outer", 0x0021, Some("java/lang/Object"), &[], &[]);
        let inner = class_bytes(
            "demo/Outer$Inner",
            0x0021,
            Some("java/lang/Object"),
            &[],
            &[],
        );
        let anonymous = class_bytes("demo/Outer$1", 0x0021, Some("java/lang/Object"), &[], &[]);
        let jar = write_stored_zip(&[
            ("demo/Outer.class", &outer),
            ("demo/Outer$Inner.class", &inner),
            ("demo/Outer$1.class", &anonymous),
        ]);
        let path = std::env::temp_dir().join(format!(
            "java-lsp-classfile-nested-{}-{}.jar",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, &jar).unwrap();

        let entries = entries_from_jar(&path).unwrap();
        let outer_entry = entries
            .iter()
            .find(|entry| entry.name == "Outer" && entry.kind == IndexKind::Class)
            .unwrap();
        assert!(outer_entry.container.is_empty());
        let inner_entry = entries.iter().find(|entry| entry.name == "Inner").unwrap();
        assert_eq!(inner_entry.container, vec!["Outer".to_string()]);
        // Anonymous classes ($1) are not indexable names.
        assert!(!entries.iter().any(|entry| entry.name == "1"));

        let _ = std::fs::remove_file(&path);
    }
}
