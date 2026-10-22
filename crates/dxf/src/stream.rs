//! A bounded ASCII tag cursor. Large native imports replay a seekable spool without keeping
//! its text or all numeric group codes in memory. The ordinary byte API remains wasm-safe.

use std::io::{BufRead, Read, Seek, SeekFrom};

use crate::{DxfError, Kind, MAX_TAGS, Result, Tag, Value, codepage, decode_text_escapes, kind_of, typed};

const MAX_LINE: usize = 8 << 20;
const HEADER_SCAN: usize = 1 << 20;

pub struct AsciiReader<R> {
    input: R,
    decoder: codepage::Decoder,
    buffer: Vec<u8>,
    line: usize,
    count: usize,
    ended: bool,
}

fn line(input: &mut impl BufRead, out: &mut Vec<u8>) -> Result<usize> {
    out.clear();
    let n = input.take(MAX_LINE as u64 + 1).read_until(b'\n', out)?;
    if n > MAX_LINE {
        return Err(DxfError::LineTooLong { limit: MAX_LINE });
    }
    Ok(n)
}

impl<R: BufRead + Seek> AsciiReader<R> {
    pub fn new(mut input: R) -> Result<Self> {
        let mut utf8 = true;
        let mut header = Vec::new();
        let mut buffer = Vec::new();
        let mut in_header = true;
        // Whole-file UTF-8 preference matches parse(): a single invalid line switches the
        // complete file to its declared code page, rather than mixing decoders per record.
        while line(&mut input, &mut buffer)? != 0 {
            utf8 &= std::str::from_utf8(&buffer).is_ok();
            if in_header && header.len().saturating_add(buffer.len()) <= HEADER_SCAN {
                header.extend_from_slice(&buffer);
            }
            in_header &= buffer.trim_ascii() != b"ENDSEC";
        }
        let decoder = if utf8 {
            codepage::Decoder::Utf8
        } else {
            codepage::ascii_code_page(&header)
                .and_then(|name| std::str::from_utf8(name).ok())
                .and_then(codepage::decoder)
                .unwrap_or(codepage::Decoder::Utf8)
        };
        input.seek(SeekFrom::Start(0))?;
        Ok(Self { input, decoder, buffer, line: 0, count: 0, ended: false })
    }

    pub fn rewind(&mut self) -> Result<()> {
        self.input.seek(SeekFrom::Start(0))?;
        self.line = 0;
        self.count = 0;
        self.ended = false;
        Ok(())
    }

    pub fn next_tag(&mut self) -> Result<Option<Tag>> {
        if self.ended {
            return Ok(None);
        }
        let code = loop {
            if line(&mut self.input, &mut self.buffer)? == 0 {
                if self.count == 0 {
                    return Err(DxfError::NotDxf);
                }
                return Ok(None);
            }
            self.line = self.line.saturating_add(1);
            let text = String::from_utf8_lossy(&self.buffer);
            let code = text.trim();
            if !code.is_empty() {
                break code.parse::<i32>().map_err(|_| DxfError::BadCode { line: self.line, text: code.chars().take(40).collect() })?;
            }
        };
        if line(&mut self.input, &mut self.buffer)? == 0 {
            return Err(DxfError::Eof);
        }
        self.line = self.line.saturating_add(1);
        self.count = self.count.saturating_add(1);
        if self.count > MAX_TAGS {
            return Err(DxfError::TooLarge { tags: self.count, limit: MAX_TAGS });
        }
        let text = codepage::decode(&self.buffer, self.decoder);
        let text = text.trim_end_matches(['\r', '\n']);
        let value = if kind_of(code) == Kind::Str { Value::Str(decode_text_escapes(text)) } else { typed(code, text) };
        self.ended = code == 0 && matches!(&value, Value::Str(s) if s == "EOF");
        Ok(Some(Tag { code, value }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn streamed_tags_match_utf8_and_legacy_code_pages() {
        let utf8 = b"0\nSECTION\n2\nHEADER\n9\n$DWGCODEPAGE\n3\nANSI_1251\n0\nENDSEC\n0\nSECTION\n2\nENTITIES\n0\nTEXT\n1\nCaf\xc3\xa9 \\U+03A9\n10\n1.25\n70\n2\n0\nENDSEC\n0\nEOF\n";
        let legacy = b"0\nSECTION\n2\nHEADER\n9\n$DWGCODEPAGE\n3\nANSI_1251\n0\nENDSEC\n0\nSECTION\n2\nENTITIES\n0\nTEXT\n1\n\xcf\xf0\xe8\xe2\xe5\xf2\n0\nENDSEC\n0\nEOF\n";
        for bytes in [utf8.as_slice(), legacy.as_slice()] {
            let mut reader = AsciiReader::new(Cursor::new(bytes)).unwrap();
            for _ in 0..2 {
                let mut actual = Vec::new();
                while let Some(tag) = reader.next_tag().unwrap() {
                    actual.push(tag);
                }
                assert_eq!(actual, crate::parse(bytes).unwrap());
                reader.rewind().unwrap();
            }
        }
    }

    #[test]
    fn truncated_and_oversized_lines_return_errors() {
        let mut reader = AsciiReader::new(Cursor::new(b"0\nSECTION\n2\n")).unwrap();
        assert!(reader.next_tag().unwrap().is_some());
        assert!(matches!(reader.next_tag(), Err(DxfError::Eof)));
        assert!(matches!(AsciiReader::new(Cursor::new(vec![b'x'; MAX_LINE + 1])), Err(DxfError::LineTooLong { .. })));
    }
}
