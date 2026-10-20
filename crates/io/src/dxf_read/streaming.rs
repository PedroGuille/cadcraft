//! Two-pass native import: retain symbol/object metadata and foreign extras, then consume
//! graphical entities directly into COW stores. Numeric coordinate tags live for one entity
//! sequence, rather than for the complete drawing alongside its decoded objects.

use std::io::{BufRead, Seek};

use super::*;

const MAX_RECORD_TAGS: usize = 1 << 20;

struct Records<R> {
    tags: cadcraft_dxf::AsciiReader<R>,
    pending: Option<Tag>,
}

impl<R: BufRead + Seek> Records<R> {
    fn next(&mut self) -> Result<Option<(String, Vec<Tag>)>> {
        let first = match self.pending.take() {
            Some(tag) => Some(tag),
            None => self.tags.next_tag().map_err(format)?,
        };
        let Some(first) = first else { return Ok(None) };
        let mut body = Vec::new();
        let kind = if first.code == 0 {
            first.str()
        } else {
            body.push(first);
            String::new()
        };
        while let Some(tag) = self.tags.next_tag().map_err(format)? {
            if tag.code == 0 {
                self.pending = Some(tag);
                break;
            }
            if body.len() >= MAX_RECORD_TAGS {
                return Err(IoError::Format("a DXF record exceeds the native import limit".into()));
            }
            body.push(tag);
        }
        Ok(Some((kind, body)))
    }
}

fn format(error: impl std::fmt::Display) -> IoError {
    IoError::Format(error.to_string())
}

fn append(out: &mut Vec<Tag>, kind: String, body: impl IntoIterator<Item = Tag>) {
    if !kind.is_empty() {
        out.push(Tag::s(0, kind));
    }
    out.extend(body);
}

/// Preserve everything raw-data readers use, including subclass boundaries and complete
/// application groups. Geometry, widths, labels and attribute values are consumed in pass 2.
fn extras(kind: &str, tags: Vec<Tag>) -> Vec<Tag> {
    let needed = kind == "IMAGE" || tags.iter().any(|t| t.code >= 1000 || matches!(t.code, 102 | 347 | 284 | 430));
    if !needed {
        return Vec::new();
    }
    let mut group = false;
    tags.into_iter()
        .filter(|tag| {
            let was_group = group;
            if tag.code == 102 {
                group = tag.str().trim().starts_with('{');
            }
            was_group
                || group
                || tag.code >= 1000
                || matches!(tag.code, 5 | 105 | 100 | 102 | 330 | 347 | 284 | 420 | 430)
                || (kind == "IMAGE" && tag.code == 340)
        })
        .collect()
}

pub(crate) fn read<R: BufRead + Seek>(input: R, overrides: &[(&str, i32, String)]) -> Result<Drawing> {
    let mut records = Records { tags: cadcraft_dxf::AsciiReader::new(input).map_err(format)?, pending: None };
    let mut metadata = Vec::new();
    let mut section = String::new();
    let mut maximum: Option<Handle> = None;
    while let Some((kind, body)) = records.next()? {
        for handle in body.iter().filter(|t| t.code == 5 || t.code == 105).filter_map(|t| Handle::parse_hex(&t.str())) {
            maximum = Some(maximum.map_or(handle, |h| if h.0 >= handle.0 { h } else { handle }));
        }
        if kind == "SECTION" {
            section = T(&body).s(2).unwrap_or_default();
            let mut body = body;
            if section == "HEADER" {
                for (name, code, value) in overrides {
                    if !body.iter().any(|t| t.code == 9 && t.str() == *name) {
                        body.push(Tag::s(9, *name));
                        body.push(if *code == 70 { Tag::i(*code, value.parse().unwrap_or(0)) } else { Tag::f(*code, value.parse().unwrap_or(0.0)) });
                    }
                }
            }
            append(&mut metadata, kind, body);
        } else if kind == "ENDSEC" || kind == "EOF" {
            append(&mut metadata, kind, body);
            section.clear();
        } else if matches!(section.as_str(), "ENTITIES" | "BLOCKS") && !matches!(kind.as_str(), "BLOCK" | "ENDBLK") {
            let extra = extras(&kind, body);
            if !extra.is_empty() {
                append(&mut metadata, kind, extra);
            }
        } else {
            append(&mut metadata, kind, body);
        }
    }
    records.tags.rewind().map_err(format)?;
    records.pending = None;
    super::read_tags(&metadata, maximum, Some(&mut |d, rx, paper, first, skipped| load(&mut records, d, rx, paper, first, skipped)))
}

fn load<R: BufRead + Seek>(
    records: &mut Records<R>,
    d: &mut Drawing,
    rx: &mut Rx,
    paper_for: &HashMap<String, String>,
    first_layout: &str,
    skipped: &mut std::collections::BTreeMap<String, usize>,
) -> Result<()> {
    let mut section = String::new();
    let mut block: Option<String> = None;
    let mut bundle = Vec::new();
    let mut bundle_tags = 0usize;
    let mut seen = std::collections::HashSet::new();
    while let Some((kind, body)) = records.next()? {
        let continuation = matches!(kind.as_str(), "VERTEX" | "ATTRIB" | "SEQEND");
        if !continuation {
            flush(&mut bundle, &section, block.as_deref(), &mut seen, d, rx, paper_for, first_layout, skipped);
            bundle_tags = 0;
        }
        match kind.as_str() {
            "SECTION" => {
                section = T(&body).s(2).unwrap_or_default();
                block = None;
                seen.clear();
            }
            "ENDSEC" | "EOF" => {
                section.clear();
                block = None;
            }
            "BLOCK" if section == "BLOCKS" => {
                block = Some(T(&body).s(2).unwrap_or_default());
                seen.clear();
            }
            "ENDBLK" if section == "BLOCKS" => {
                block = None;
            }
            _ if section == "ENTITIES" || (section == "BLOCKS" && block.is_some()) => {
                bundle_tags = bundle_tags.saturating_add(body.len());
                bundle.push((kind, body));
                if bundle_tags > MAX_RECORD_TAGS {
                    return Err(IoError::Format("a DXF entity sequence exceeds the native import limit".into()));
                }
            }
            _ => {}
        }
    }
    flush(&mut bundle, &section, block.as_deref(), &mut seen, d, rx, paper_for, first_layout, skipped);
    Ok(())
}

fn flush(
    bundle: &mut Vec<(String, Vec<Tag>)>,
    section: &str,
    block: Option<&str>,
    seen: &mut std::collections::HashSet<Handle>,
    d: &mut Drawing,
    rx: &mut Rx,
    paper_for: &HashMap<String, String>,
    first_layout: &str,
    skipped: &mut std::collections::BTreeMap<String, usize>,
) {
    if bundle.is_empty() {
        return;
    }
    let refs: Vec<_> = bundle.iter().map(|(kind, tags)| (kind.clone(), tags.as_slice())).collect();
    let mut missing = 0;
    for (owner, paper, mut entity) in parse_entities(&refs, d, rx, &mut missing) {
        if !seen.insert(entity.handle) {
            entity.handle = d.new_handle();
            seen.insert(entity.handle);
        }
        let block_name = block.map(str::to_ascii_uppercase);
        if section == "BLOCKS" && block_name.as_deref().is_some_and(|name| name != "*MODEL_SPACE" && !name.starts_with("*PAPER_SPACE")) {
            if let Some(block) = block.and_then(|name| d.blocks.get_mut(name))
                && block.xref_path.is_none()
            {
                std::sync::Arc::make_mut(block).entities.push(entity);
            }
            continue;
        }
        let owner = if section == "BLOCKS" {
            block_name
        } else {
            owner.and_then(|o| rx.brs.get(&o.trim().to_ascii_uppercase()).cloned()).map(|n| n.to_ascii_uppercase())
        };
        d.ensure_layer(&entity.common.layer);
        if paper || owner.as_deref().is_some_and(|name| name.starts_with("*PAPER_SPACE")) {
            let name = owner.and_then(|n| paper_for.get(&n)).map_or(first_layout, String::as_str);
            if let Some(layout) = d.layouts.iter_mut().find(|l| l.name == name) {
                layout.entities.push(entity);
            }
        } else {
            d.model.push(entity);
        }
    }
    count_skipped(skipped, section, missing);
    bundle.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn native_stream_preserves_complete_drawing_and_foreign_data() {
        let mut drawing = crate::tests::sample();
        let entity = drawing.model.iter().next().unwrap().handle;
        drawing.model.modify(entity, |e| {
            e.common.xdata = vec![RawTag { code: 1001, value: "THIRD_PARTY".into() }, RawTag { code: 1000, value: "kept café".into() }]
        });
        std::sync::Arc::make_mut(&mut drawing.foreign).records.insert(
            ("LAYER".into(), "WALLS".into()),
            cadcraft_doc::foreign::ForeignExtra { props: vec![RawTag { code: 347, value: "0".into() }], ..Default::default() },
        );
        for version in [crate::Version::R2000, crate::Version::R2007, crate::Version::R2018] {
            let bytes = crate::write_with(&drawing, "drawing.dxf", &crate::SaveOptions { version: Some(version), binary: false }).unwrap();
            let expected = super::super::read(&bytes).unwrap();
            let actual = read(Cursor::new(&bytes), &[]).unwrap();
            assert_eq!(actual, expected, "{version:?}");
        }
    }

    #[test]
    fn native_stream_keeps_polyline_sequences_attributes_and_duplicate_handles() {
        let bytes = b"0\nSECTION\n2\nENTITIES\n0\nPOLYLINE\n5\n20\n70\n0\n0\nVERTEX\n10\n1\n20\n2\n0\nVERTEX\n10\n3\n20\n4\n0\nSEQEND\n0\nINSERT\n5\n21\n2\nX\n66\n1\n0\nATTRIB\n2\nA\n1\nVALUE\n0\nSEQEND\n0\nLINE\n5\n22\n10\n1\n11\n2\n0\nLINE\n5\n22\n10\n3\n11\n4\n0\nENDSEC\n0\nEOF\n";
        assert_eq!(read(Cursor::new(bytes), &[]).unwrap(), super::super::read(bytes).unwrap());
    }

    #[test]
    fn streamed_dwg_header_variables_match_the_byte_bridge() {
        let bytes = crate::write(&crate::tests::sample(), "drawing.dwg").unwrap();
        let (dxf, _) = cadcraft_dwg::dwg_to_dxf_noted(&bytes, cadcraft_dwg::Limits::DEFAULT).unwrap();
        let expected = super::super::read(&dxf).unwrap();
        let mut stream = Vec::new();
        let (_, header) = cadcraft_dwg::dwg_to_dxf_writer(&bytes, cadcraft_dwg::Limits::DEFAULT, &mut stream).unwrap();
        let actual = read(Cursor::new(stream), &header).unwrap();
        assert_eq!(actual, expected);
        assert!(
            cadcraft_dwg::dwg_to_dxf_writer(&bytes, cadcraft_dwg::Limits { max_dxf_bytes: 128, ..cadcraft_dwg::Limits::DEFAULT }, &mut Vec::new())
                .is_err()
        );
        // Padding a valid synthetic DWG exercises the public native threshold and disposable
        // file path without retaining a large binary fixture in the repository.
        let mut padded = bytes;
        padded.resize(4 << 20, 0);
        assert_eq!(crate::read(&padded, "large.dwg").unwrap(), expected);
    }
}
