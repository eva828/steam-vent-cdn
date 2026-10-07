//! Minimal reader for Steam's binary KeyValues format.
//!
//! Only the parts needed to inspect PICS app info are implemented. Unknown type
//! bytes, truncated input, invalid encodings and trailing data are reported as
//! errors instead of being silently ignored.

use crate::error::{Result, VdfError};

const TYPE_OBJECT: u8 = 0x00;
const TYPE_STRING: u8 = 0x01;
const TYPE_INT32: u8 = 0x02;
const TYPE_FLOAT: u8 = 0x03;
const TYPE_POINTER: u8 = 0x04;
const TYPE_WSTRING: u8 = 0x05;
const TYPE_COLOR: u8 = 0x06;
const TYPE_UINT64: u8 = 0x07;
const TYPE_END: u8 = 0x08;

/// A single key/value pair at the root of a binary KeyValues document.
#[derive(Debug, Clone, PartialEq)]
pub struct Vdf {
    pub key: String,
    pub value: VdfValue,
}

impl Vdf {
    /// Traverse nested objects starting from the root value.
    pub fn get_path(&self, path: &[&str]) -> Option<&VdfValue> {
        let mut current = &self.value;
        for segment in path {
            current = current.as_object()?.get(segment)?;
        }
        Some(current)
    }
}

/// An ordered collection of key/value pairs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VdfObject {
    entries: Vec<(String, VdfValue)>,
}

impl VdfObject {
    pub fn get(&self, key: &str) -> Option<&VdfValue> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// Traverse nested objects starting from this object.
    pub fn get_path(&self, path: &[&str]) -> Option<&VdfValue> {
        let (first, rest) = path.split_first()?;
        let mut value = self.get(first)?;
        for segment in rest {
            value = value.as_object()?.get(segment)?;
        }
        Some(value)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &VdfValue)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// A binary KeyValues value.
#[derive(Debug, Clone, PartialEq)]
pub enum VdfValue {
    Object(VdfObject),
    String(String),
    Int32(i32),
    Float(f32),
    Pointer(u32),
    WString(String),
    Color([u8; 4]),
    UInt64(u64),
}

impl VdfValue {
    pub fn type_name(&self) -> &'static str {
        match self {
            VdfValue::Object(_) => "object",
            VdfValue::String(_) => "string",
            VdfValue::Int32(_) => "int32",
            VdfValue::Float(_) => "float",
            VdfValue::Pointer(_) => "pointer",
            VdfValue::WString(_) => "wstring",
            VdfValue::Color(_) => "color",
            VdfValue::UInt64(_) => "uint64",
        }
    }

    pub fn as_object(&self) -> Option<&VdfObject> {
        match self {
            VdfValue::Object(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            VdfValue::String(value) | VdfValue::WString(value) => Some(value),
            _ => None,
        }
    }

    /// Interpret the value as an unsigned integer.
    ///
    /// Steam stores many numeric fields as strings, so those are parsed as well.
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            VdfValue::UInt64(value) => Some(*value),
            VdfValue::Int32(value) if *value >= 0 => Some(*value as u64),
            VdfValue::Pointer(value) => Some(*value as u64),
            VdfValue::String(value) | VdfValue::WString(value) => value.trim().parse().ok(),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self.as_u64() {
            Some(value) => Some(value != 0),
            None => match self.as_str()? {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            },
        }
    }
}

/// Parse a binary KeyValues document that consists of a single root pair.
pub fn parse_binary(bytes: &[u8]) -> Result<Vdf> {
    let mut parser = Parser {
        data: bytes,
        pos: 0,
    };

    let type_byte = parser.read_u8()?;
    if type_byte != TYPE_OBJECT && type_byte != TYPE_STRING {
        return Err(VdfError::UnknownType {
            value: type_byte,
            offset: 0,
        }
        .into());
    }

    let key = parser.read_cstring()?;
    let value = parser.read_value(type_byte)?;

    if parser.pos != bytes.len() {
        return Err(VdfError::TrailingData {
            remaining: bytes.len() - parser.pos,
        }
        .into());
    }

    Ok(Vdf { key, value })
}

struct Parser<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn read_u8(&mut self) -> Result<u8> {
        let value = *self
            .data
            .get(self.pos)
            .ok_or(VdfError::UnexpectedEof { offset: self.pos })?;
        self.pos += 1;
        Ok(value)
    }

    fn read_exact(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or(VdfError::UnexpectedEof { offset: self.pos })?;
        let slice = self
            .data
            .get(self.pos..end)
            .ok_or(VdfError::UnexpectedEof { offset: self.pos })?;
        self.pos = end;
        Ok(slice)
    }

    fn read_cstring(&mut self) -> Result<String> {
        let start = self.pos;
        let end = self.data[start..]
            .iter()
            .position(|b| *b == 0)
            .map(|offset| start + offset)
            .ok_or(VdfError::UnexpectedEof { offset: start })?;
        let raw = &self.data[start..end];
        self.pos = end + 1;
        let value = std::str::from_utf8(raw)
            .map_err(|_| VdfError::InvalidUtf8 { offset: start })?
            .to_string();
        Ok(value)
    }

    fn read_utf16_string(&mut self) -> Result<String> {
        let start = self.pos;
        let mut units = Vec::new();
        loop {
            let unit = u16::from_le_bytes(
                self.read_exact(2)?
                    .try_into()
                    .map_err(|_| VdfError::UnexpectedEof { offset: start })?,
            );
            if unit == 0 {
                break;
            }
            units.push(unit);
        }
        String::from_utf16(&units).map_err(|_| VdfError::InvalidUtf16 { offset: start }.into())
    }

    fn read_value(&mut self, type_byte: u8) -> Result<VdfValue> {
        match type_byte {
            TYPE_OBJECT => Ok(VdfValue::Object(self.read_object()?)),
            TYPE_STRING => Ok(VdfValue::String(self.read_cstring()?)),
            TYPE_INT32 => {
                let bytes = self.read_exact(4)?;
                Ok(VdfValue::Int32(i32::from_le_bytes(
                    bytes.try_into().unwrap(),
                )))
            }
            TYPE_FLOAT => {
                let bytes = self.read_exact(4)?;
                Ok(VdfValue::Float(f32::from_le_bytes(
                    bytes.try_into().unwrap(),
                )))
            }
            TYPE_POINTER => {
                let bytes = self.read_exact(4)?;
                Ok(VdfValue::Pointer(u32::from_le_bytes(
                    bytes.try_into().unwrap(),
                )))
            }
            TYPE_WSTRING => Ok(VdfValue::WString(self.read_utf16_string()?)),
            TYPE_COLOR => {
                let bytes = self.read_exact(4)?;
                Ok(VdfValue::Color(bytes.try_into().unwrap()))
            }
            TYPE_UINT64 => {
                let bytes = self.read_exact(8)?;
                Ok(VdfValue::UInt64(u64::from_le_bytes(
                    bytes.try_into().unwrap(),
                )))
            }
            other => Err(VdfError::UnknownType {
                value: other,
                offset: self.pos,
            }
            .into()),
        }
    }

    fn read_object(&mut self) -> Result<VdfObject> {
        let mut entries = Vec::new();
        loop {
            let type_byte = self.read_u8()?;
            if type_byte == TYPE_END {
                break;
            }
            let key = self.read_cstring()?;
            let value = self.read_value(type_byte)?;
            entries.push((key, value));
        }
        Ok(VdfObject { entries })
    }
}

/// Parse a text (v1) KeyValues document.
///
/// Comments (`//`), quoted strings with the usual escapes and unquoted tokens
/// are supported. The root pair may hold either a string or an object.
pub fn parse_text(bytes: &[u8]) -> Result<Vdf> {
    let raw = std::str::from_utf8(bytes).map_err(|_| VdfError::InvalidUtf8 { offset: 0 })?;
    let input = raw.strip_prefix('\u{feff}').unwrap_or(raw);

    let mut parser = TextParser::new(input);
    parser.skip_ws_and_comments();
    let key = parser.parse_key()?;
    let value = parser.parse_value()?;
    parser.skip_ws_and_comments();
    // Some PICS buffers are NUL terminated.
    while parser.peek() == Some('\0') {
        parser.pos += 1;
    }

    if parser.pos != input.len() {
        return Err(VdfError::TrailingData {
            remaining: input.len() - parser.pos,
        }
        .into());
    }

    Ok(Vdf { key, value })
}

/// Parse a KeyValues document, detecting the text and binary encodings.
pub fn parse(bytes: &[u8]) -> Result<Vdf> {
    let mut index = 0;
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        index = 3;
    }
    while index < bytes.len() && matches!(bytes[index], b' ' | b'\t' | b'\r' | b'\n') {
        index += 1;
    }

    match bytes.get(index) {
        Some(b'"') | Some(b'{') | Some(b'/') => parse_text(bytes),
        Some(b) if b.is_ascii_alphabetic() || *b == b'_' => parse_text(bytes),
        Some(&TYPE_OBJECT) | Some(&TYPE_STRING) => parse_binary(bytes),
        Some(value) => Err(VdfError::UnknownType {
            value: *value,
            offset: index,
        }
        .into()),
        None => Err(VdfError::UnexpectedEof { offset: index }.into()),
    }
}

struct TextParser<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> TextParser<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, pos: 0 }
    }

    fn rest(&self) -> &'a str {
        &self.input[self.pos..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn skip_ws_and_comments(&mut self) {
        loop {
            let rest = self.rest();
            let trimmed = rest.trim_start_matches(char::is_whitespace);
            self.pos += rest.len() - trimmed.len();

            if self.rest().starts_with("//") {
                match self.rest().find('\n') {
                    Some(offset) => self.pos += offset + 1,
                    None => self.pos = self.input.len(),
                }
            } else {
                break;
            }
        }
    }

    fn parse_key(&mut self) -> Result<String> {
        self.skip_ws_and_comments();

        match self.peek() {
            Some('"') => self.parse_quoted(),
            Some(c) if c != '}' => {
                let rest = self.rest();
                let end = rest
                    .find(|c: char| c.is_whitespace() || c == '{' || c == '}')
                    .unwrap_or(rest.len());
                self.pos += end;
                Ok(rest[..end].to_string())
            }
            _ => Err(VdfError::UnexpectedEof { offset: self.pos }.into()),
        }
    }

    fn parse_quoted(&mut self) -> Result<String> {
        self.pos += 1; // opening quote
        let mut out = String::new();

        loop {
            match self.peek() {
                None => return Err(VdfError::UnexpectedEof { offset: self.pos }.into()),
                Some('"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some('\\') => {
                    self.pos += 1;
                    match self.peek() {
                        None => return Err(VdfError::UnexpectedEof { offset: self.pos }.into()),
                        Some('n') => {
                            out.push('\n');
                            self.pos += 1;
                        }
                        Some('t') => {
                            out.push('\t');
                            self.pos += 1;
                        }
                        Some('r') => {
                            out.push('\r');
                            self.pos += 1;
                        }
                        Some('\\') => {
                            out.push('\\');
                            self.pos += 1;
                        }
                        Some('"') => {
                            out.push('"');
                            self.pos += 1;
                        }
                        Some(other) => {
                            out.push(other);
                            self.pos += other.len_utf8();
                        }
                    }
                }
                Some(c) => {
                    out.push(c);
                    self.pos += c.len_utf8();
                }
            }
        }
    }

    fn parse_value(&mut self) -> Result<VdfValue> {
        self.skip_ws_and_comments();
        if self.peek() == Some('{') {
            self.pos += 1;
            Ok(VdfValue::Object(self.parse_object()?))
        } else {
            Ok(VdfValue::String(self.parse_key()?))
        }
    }

    fn parse_object(&mut self) -> Result<VdfObject> {
        let mut entries = Vec::new();

        loop {
            self.skip_ws_and_comments();
            match self.peek() {
                Some('}') => {
                    self.pos += 1;
                    break;
                }
                None => return Err(VdfError::UnexpectedEof { offset: self.pos }.into()),
                _ => {}
            }

            let key = self.parse_key()?;
            let value = self.parse_value()?;
            entries.push((key, value));
        }

        Ok(VdfObject { entries })
    }
}

impl VdfObject {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Insert a key, replacing the value when the key already exists.
    pub fn insert(&mut self, key: impl Into<String>, value: VdfValue) {
        let key = key.into();
        match self
            .entries
            .iter_mut()
            .find(|(existing, _)| *existing == key)
        {
            Some(entry) => entry.1 = value,
            None => self.entries.push((key, value)),
        }
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut VdfValue> {
        self.entries
            .iter_mut()
            .find(|(existing, _)| existing == key)
            .map(|(_, value)| value)
    }

    pub fn remove(&mut self, key: &str) -> Option<VdfValue> {
        let index = self
            .entries
            .iter()
            .position(|(existing, _)| existing == key)?;
        Some(self.entries.remove(index).1)
    }
}

impl VdfValue {
    pub fn string(value: impl Into<String>) -> Self {
        VdfValue::String(value.into())
    }

    pub fn uint64(value: u64) -> Self {
        VdfValue::UInt64(value)
    }

    pub fn object(entries: Vec<(String, VdfValue)>) -> Self {
        VdfValue::Object(VdfObject { entries })
    }

    pub fn as_object_mut(&mut self) -> Option<&mut VdfObject> {
        match self {
            VdfValue::Object(value) => Some(value),
            _ => None,
        }
    }

    fn as_text(&self) -> String {
        match self {
            VdfValue::String(value) | VdfValue::WString(value) => value.clone(),
            VdfValue::Int32(value) => value.to_string(),
            VdfValue::UInt64(value) => value.to_string(),
            VdfValue::Float(value) => value.to_string(),
            VdfValue::Pointer(value) => value.to_string(),
            VdfValue::Color(value) => format!(
                "{:02X}{:02X}{:02X}{:02X}",
                value[0], value[1], value[2], value[3]
            ),
            VdfValue::Object(_) => String::new(),
        }
    }
}

/// Escape a string for the text KeyValues format.
pub fn escape_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out
}

/// Serialize a document to the text KeyValues format used by ACF files.
pub fn to_text(vdf: &Vdf) -> String {
    let mut out = String::new();
    write_pair(&mut out, &vdf.key, &vdf.value, 0);
    out
}

fn write_pair(out: &mut String, key: &str, value: &VdfValue, depth: usize) {
    let indent = "\t".repeat(depth);
    match value {
        VdfValue::Object(object) => {
            out.push_str(&format!("{indent}\"{}\"\n{indent}{{\n", escape_text(key)));
            for (child_key, child) in object.iter() {
                write_pair(out, child_key, child, depth + 1);
            }
            out.push_str(&format!("{indent}}}\n"));
        }
        scalar => {
            out.push_str(&format!(
                "{indent}\"{}\"\t\t\"{}\"\n",
                escape_text(key),
                escape_text(&scalar.as_text())
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cstr(s: &str) -> Vec<u8> {
        let mut out = s.as_bytes().to_vec();
        out.push(0);
        out
    }

    fn document() -> Vec<u8> {
        let mut data = Vec::new();
        // root object "appinfo"
        data.push(TYPE_OBJECT);
        data.extend(cstr("appinfo"));
        // "appid" "440"
        data.push(TYPE_STRING);
        data.extend(cstr("appid"));
        data.extend(cstr("440"));
        // "depots" { "441" { "manifests" { "public" { "gid" "12345" } } } }
        data.push(TYPE_OBJECT);
        data.extend(cstr("depots"));
        data.push(TYPE_OBJECT);
        data.extend(cstr("441"));
        data.push(TYPE_OBJECT);
        data.extend(cstr("manifests"));
        data.push(TYPE_OBJECT);
        data.extend(cstr("public"));
        data.push(TYPE_STRING);
        data.extend(cstr("gid"));
        data.extend(cstr("12345"));
        data.push(TYPE_END); // public
        data.push(TYPE_END); // manifests
        data.push(TYPE_END); // 441
        data.push(TYPE_END); // depots
        data.push(TYPE_END); // appinfo
        data
    }

    #[test]
    fn parses_nested_document() {
        let vdf = parse_binary(&document()).unwrap();
        assert_eq!(vdf.key, "appinfo");
        assert_eq!(vdf.get_path(&["appid"]).unwrap().as_str(), Some("440"));
        assert_eq!(
            vdf.get_path(&["depots", "441", "manifests", "public", "gid"])
                .unwrap()
                .as_u64(),
            Some(12345)
        );
    }

    #[test]
    fn parses_scalar_types() {
        let mut data = Vec::new();
        data.push(TYPE_OBJECT);
        data.extend(cstr("root"));
        data.push(TYPE_INT32);
        data.extend(cstr("i"));
        data.extend((-7i32).to_le_bytes());
        data.push(TYPE_UINT64);
        data.extend(cstr("u"));
        data.extend(9_000_000_000u64.to_le_bytes());
        data.push(TYPE_POINTER);
        data.extend(cstr("p"));
        data.extend(42u32.to_le_bytes());
        data.push(TYPE_END);

        let vdf = parse_binary(&data).unwrap();
        let root = vdf.value.as_object().unwrap();
        assert_eq!(root.get("i").unwrap().as_u64(), None);
        assert_eq!(root.get("i").unwrap(), &VdfValue::Int32(-7));
        assert_eq!(root.get("u").unwrap().as_u64(), Some(9_000_000_000));
        assert_eq!(root.get("p").unwrap().as_u64(), Some(42));
    }

    #[test]
    fn rejects_trailing_data() {
        let mut data = document();
        data.push(0xFF);
        assert!(matches!(
            parse_binary(&data),
            Err(crate::error::SteamVentError::Vdf(
                VdfError::TrailingData { .. }
            ))
        ));
    }

    #[test]
    fn rejects_unknown_type() {
        let mut data = Vec::new();
        data.push(TYPE_OBJECT);
        data.extend(cstr("root"));
        data.push(0x7F);
        data.extend(cstr("bad"));
        assert!(matches!(
            parse_binary(&data),
            Err(crate::error::SteamVentError::Vdf(VdfError::UnknownType {
                value: 0x7F,
                ..
            }))
        ));
    }

    #[test]
    fn rejects_truncated_object() {
        let mut data = document();
        data.pop(); // remove the root object terminator
        assert!(matches!(
            parse_binary(&data),
            Err(crate::error::SteamVentError::Vdf(
                VdfError::UnexpectedEof { .. }
            ))
        ));
    }

    #[test]
    fn as_bool_accepts_string_and_int() {
        assert_eq!(VdfValue::String("1".into()).as_bool(), Some(true));
        assert_eq!(VdfValue::String("false".into()).as_bool(), Some(false));
        assert_eq!(VdfValue::Int32(0).as_bool(), Some(false));
    }

    #[test]
    fn parses_text_document() {
        let text = r#"
"appinfo"
{
    "appid"        "480"
    "common"
    {
        "name"    "Spacewar"
    }
    "depots"
    {
        "branches" { "public" { "buildid" "123" } }
        "481"
        {
            "manifests" { "public" { "gid" "999" "size" "42" } }
            "config" { "oslist" "linux" }
        }
    }
}
"#;
        let vdf = parse_text(text.as_bytes()).unwrap();
        assert_eq!(vdf.key, "appinfo");
        assert_eq!(vdf.get_path(&["appid"]).unwrap().as_u64(), Some(480));
        assert_eq!(
            vdf.get_path(&["common", "name"]).unwrap().as_str(),
            Some("Spacewar")
        );
        assert_eq!(
            vdf.get_path(&["depots", "branches", "public", "buildid"])
                .unwrap()
                .as_u64(),
            Some(123)
        );
        assert_eq!(
            vdf.get_path(&["depots", "481", "manifests", "public", "gid"])
                .unwrap()
                .as_u64(),
            Some(999)
        );
        assert_eq!(
            vdf.get_path(&["depots", "481", "config", "oslist"])
                .unwrap()
                .as_str(),
            Some("linux")
        );
    }

    #[test]
    fn text_parser_handles_comments_and_escapes() {
        let text = "// leading comment\n\"root\" { \"a\" \"line\\nbreak\" } // trailing\n";
        let vdf = parse_text(text.as_bytes()).unwrap();
        assert_eq!(vdf.get_path(&["a"]).unwrap().as_str(), Some("line\nbreak"));
    }

    #[test]
    fn parse_autodetects_text_and_binary() {
        let text = "\"appinfo\" { \"appid\" \"1\" }";
        assert_eq!(parse(text.as_bytes()).unwrap().key, "appinfo");

        let binary = document();
        assert_eq!(parse(&binary).unwrap().key, "appinfo");
    }

    #[test]
    fn parse_autodetects_unquoted_root() {
        let text = "appinfo { appid 1 }";
        assert_eq!(parse(text.as_bytes()).unwrap().key, "appinfo");
    }

    #[test]
    fn text_parser_rejects_trailing_data() {
        let text = "\"root\" { \"a\" \"b\" } extra";
        assert!(matches!(
            parse_text(text.as_bytes()),
            Err(crate::error::SteamVentError::Vdf(
                VdfError::TrailingData { .. }
            ))
        ));
    }

    #[test]
    fn serializes_and_reparses_text() {
        let mut root = VdfObject::new();
        root.insert("appid", VdfValue::string("480"));
        root.insert(
            "InstalledDepots",
            VdfValue::object(vec![(
                "481".to_string(),
                VdfValue::object(vec![
                    ("manifest".to_string(), VdfValue::string("123")),
                    ("size".to_string(), VdfValue::string("456")),
                ]),
            )]),
        );

        let document = Vdf {
            key: "AppState".to_string(),
            value: VdfValue::Object(root),
        };
        let text = to_text(&document);
        let reparsed = parse_text(text.as_bytes()).unwrap();
        assert_eq!(reparsed.key, "AppState");
        assert_eq!(reparsed.get_path(&["appid"]).unwrap().as_u64(), Some(480));
        assert_eq!(
            reparsed
                .get_path(&["InstalledDepots", "481", "manifest"])
                .unwrap()
                .as_u64(),
            Some(123)
        );
    }

    #[test]
    fn escapes_values_when_serializing() {
        assert_eq!(escape_text("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }

    #[test]
    fn insert_replaces_existing_key() {
        let mut object = VdfObject::new();
        object.insert("k", VdfValue::string("1"));
        object.insert("k", VdfValue::string("2"));
        assert_eq!(object.len(), 1);
        assert_eq!(object.get("k").unwrap().as_str(), Some("2"));
        object.remove("k");
        assert!(object.is_empty());
    }
}
