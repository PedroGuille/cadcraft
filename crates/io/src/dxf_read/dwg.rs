//! Direct structured DWG import: metadata first, then one consumed entity sequence at a time.
//! Uses the existing drawing mapper for ownership, OCS, styles, foreign data and draw order.
//! No intermediate DXF bytes, temporary drawing file, or drawing-sized group vector.
use super::*;
use cadcraft_dwg::{DecodedDrawing, Field};

const MAX_RECORD_TAGS: usize = 1 << 20;

pub(crate) fn read(bytes: &[u8]) -> Result<Drawing> {
    let decoded = DecodedDrawing::read(bytes, cadcraft_dwg::Limits::DEFAULT).map_err(IoError::Format)?;
    let notes = decoded.notes().to_vec();
    let overrides = decoded.header_overrides();
    let mut metadata = Metadata::default();
    decoded.metadata(|code, value| metadata.field(code, value)).map_err(IoError::Format)?;
    metadata.finish();
    // Header additions match the compatibility bridge, without converting coordinates to text.
    if let Some(at) = metadata.tags.iter().position(|t| t.code == 0 && t.str() == "ENDSEC") {
        let mut additional = Vec::new();
        for (name, code, value) in overrides {
            if !metadata.tags.get(..at).unwrap_or_default().iter().any(|t| t.code == 9 && t.str() == name) {
                additional.push(Tag::s(9, name));
                additional.push(if code == 70 { Tag::i(code, value.parse().unwrap_or(0)) } else { Tag::f(code, value.parse().unwrap_or(0.0)) });
            }
        }
        metadata.tags.splice(at..at, additional);
    }
    let mut decoded = Some(decoded);
    let mut drawing = read_tags(
        &metadata.tags,
        metadata.maximum,
        Some(&mut |d, rx, paper, first, skipped| {
            let source = decoded.take().ok_or_else(|| IoError::Format("DWG entity stream already consumed".into()))?;
            let mut loader = Entities {
                d,
                rx,
                paper,
                first,
                skipped,
                kind: String::new(),
                body: Vec::new(),
                bundle: Vec::new(),
                tags: 0,
                block: None,
                seen: Default::default(),
            };
            source.entities(|block, code, value| loader.field(block, code, value)).map_err(IoError::Format)
        }),
    )?;
    drawing.read_notes = notes;
    Ok(drawing)
}

#[derive(Default)]
struct Metadata {
    tags: Vec<Tag>,
    kind: String,
    body: Vec<Tag>,
    section: String,
    group: bool,
    maximum: Option<Handle>,
}
impl Metadata {
    fn field(&mut self, code: i32, value: Field<'_>) -> std::result::Result<(), String> {
        if code == 0 {
            self.finish();
            self.kind = text(value).unwrap_or_default().to_string();
            self.group = false;
            return Ok(());
        }
        if matches!(code, 5 | 105) {
            let handle = match value {
                Field::Handle(h) => Some(Handle(h)),
                _ => text(value).and_then(Handle::parse_hex),
            };
            if let Some(h) = handle {
                self.maximum = Some(self.maximum.map_or(h, |old| Handle(old.0.max(h.0))));
            }
        }
        let geometry = matches!(self.section.as_str(), "BLOCKS" | "ENTITIES")
            && !matches!(self.kind.as_str(), "BLOCK" | "ENDBLK" | "SECTION" | "ENDSEC" | "EOF");
        let was_group = self.group;
        if code == 102 {
            self.group = text(value).is_some_and(|s| s.trim().starts_with('{'));
        }
        // Drop geometry before allocating tags. Preserve subclass/app-group boundaries and all
        // properties read by dxf_raw; full entity geometry is delivered during the consuming pass.
        let keep = !geometry
            || was_group
            || self.group
            || code >= 1000
            || matches!(code, 5 | 105 | 100 | 102 | 330 | 347 | 284 | 420 | 430)
            || (self.kind == "IMAGE" && code == 340);
        if keep {
            if self.body.len() >= MAX_RECORD_TAGS {
                return Err("DWG metadata record exceeds the import limit".into());
            }
            self.body.push(tag(code, value));
        }
        Ok(())
    }
    fn finish(&mut self) {
        if self.kind.is_empty() {
            return;
        }
        let kind = std::mem::take(&mut self.kind);
        let body = std::mem::take(&mut self.body);
        if kind == "SECTION" {
            self.section = T(&body).s(2).unwrap_or_default();
            streaming::append(&mut self.tags, kind, body);
        } else if kind == "ENDSEC" || kind == "EOF" {
            streaming::append(&mut self.tags, kind, body);
            self.section.clear();
        } else if matches!(self.section.as_str(), "BLOCKS" | "ENTITIES") && !matches!(kind.as_str(), "BLOCK" | "ENDBLK") {
            let extra = streaming::extras(&kind, body);
            if !extra.is_empty() {
                streaming::append(&mut self.tags, kind, extra);
            }
        } else {
            streaming::append(&mut self.tags, kind, body);
        }
    }
}

struct Entities<'a> {
    d: &'a mut Drawing,
    rx: &'a mut Rx,
    paper: &'a HashMap<String, String>,
    first: &'a str,
    skipped: &'a mut std::collections::BTreeMap<String, usize>,
    kind: String,
    body: Vec<Tag>,
    bundle: Vec<(String, Vec<Tag>)>,
    tags: usize,
    block: Option<String>,
    seen: std::collections::HashSet<Handle>,
}
impl Entities<'_> {
    fn record(&mut self) {
        let kind = std::mem::take(&mut self.kind);
        let body = std::mem::take(&mut self.body);
        if !matches!(kind.as_str(), "" | "SECTION" | "ENDSEC" | "EOF") {
            self.bundle.push((kind, body));
        }
    }
    fn field(&mut self, block: Option<&str>, code: i32, value: Field<'_>) -> std::result::Result<(), String> {
        if code == 0 {
            self.record();
            let kind = text(value).unwrap_or_default();
            if kind == "SECTION" {
                if self.block.as_deref() != block {
                    self.seen.clear();
                    self.block = block.map(str::to_string);
                }
                self.tags = 0;
            } else if kind == "ENDSEC" {
                streaming::flush(
                    &mut self.bundle,
                    if block.is_some() { "BLOCKS" } else { "ENTITIES" },
                    block,
                    &mut self.seen,
                    self.d,
                    self.rx,
                    self.paper,
                    self.first,
                    self.skipped,
                );
            }
            self.kind = kind.to_string();
        } else if self.kind != "SECTION" {
            self.tags = self.tags.saturating_add(1);
            if self.tags > MAX_RECORD_TAGS {
                return Err("DWG entity sequence exceeds the import limit".into());
            }
            self.body.push(tag(code, value));
        }
        Ok(())
    }
}

fn text(value: Field<'_>) -> Option<&str> {
    match value {
        Field::Text(s) | Field::Xrecord(s) => Some(s),
        _ => None,
    }
}

fn tag(code: i32, value: Field<'_>) -> Tag {
    use cadcraft_dxf::Value;
    let value = match value {
        Field::Text(s) => Value::Str(string(s, false)),
        Field::Xrecord(s) => Value::Str(string(s, true)),
        Field::Real(v) => Value::Real(v),
        Field::Int(v) => Value::Int(v),
        Field::Bool(v) => Value::Bool(v),
        Field::Handle(v) => Value::Str(format!("{v:X}")),
        Field::Binary(bytes) => {
            use std::fmt::Write;
            let mut hex = String::with_capacity(bytes.len().saturating_mul(2));
            for byte in bytes {
                let _ = write!(hex, "{byte:02X}");
            }
            Value::Hex(hex)
        }
    };
    Tag::new(code, value)
}

// Retain the byte bridge's string conventions without formatting numeric fields or an
// intermediate file. DWG strings are already Unicode; only legacy escapes need decoding.
fn string(s: &str, xrecord: bool) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '^' => {
                out.push(c);
                if xrecord || chars.peek() != Some(&' ') {
                    out.push(' ');
                }
            }
            '\r' if xrecord => out.push_str("^M"),
            '\n' if xrecord => out.push_str("^J"),
            '\t' if xrecord => out.push_str("^I"),
            '\r' | '\n' => {
                if c == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push_str("\\P");
            }
            _ => out.push(c),
        }
    }
    if out.contains("\\U+") || out.contains("\\M+") { cadcraft_dxf::decode_text_escapes(&out) } else { out }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn typed_strings_keep_legacy_escape_conventions() {
        assert_eq!(string("café \\U+56FE ^3\r\nnext", false), "café 图 ^ 3\\Pnext");
        assert_eq!(string("a^ b\r\n\t", true), "a^  b^M^J^I");
    }
}
