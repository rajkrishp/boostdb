//! Just enough JSON for the results files: a pretty-printer, and a parser to read them back
//! for the summary. (The database has no dependencies, and the harness doesn't need any.)

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Value>),
    /// Fields in insertion order, so files diff cleanly.
    Obj(Vec<(String, Value)>),
}

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Obj(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Num(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn to_pretty(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, 0);
        out.push('\n');
        out
    }

    fn write(&self, out: &mut String, indent: usize) {
        let pad = |out: &mut String, n: usize| out.extend(std::iter::repeat_n("  ", n));
        match self {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Num(n) => out.push_str(&format_number(*n)),
            Value::Str(s) => write_string(out, s),
            Value::Arr(items) if items.is_empty() => out.push_str("[]"),
            Value::Arr(items) => {
                out.push_str("[\n");
                for (i, item) in items.iter().enumerate() {
                    pad(out, indent + 1);
                    item.write(out, indent + 1);
                    out.push_str(if i + 1 < items.len() { ",\n" } else { "\n" });
                }
                pad(out, indent);
                out.push(']');
            }
            Value::Obj(fields) if fields.is_empty() => out.push_str("{}"),
            Value::Obj(fields) => {
                out.push_str("{\n");
                for (i, (key, value)) in fields.iter().enumerate() {
                    pad(out, indent + 1);
                    write_string(out, key);
                    out.push_str(": ");
                    value.write(out, indent + 1);
                    out.push_str(if i + 1 < fields.len() { ",\n" } else { "\n" });
                }
                pad(out, indent);
                out.push('}');
            }
        }
    }
}

impl From<f64> for Value {
    fn from(n: f64) -> Value {
        if n.is_finite() {
            Value::Num(n)
        } else {
            Value::Null
        }
    }
}

impl From<u64> for Value {
    fn from(n: u64) -> Value {
        Value::Num(n as f64)
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Value {
        Value::Bool(b)
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Value {
        Value::Str(s.to_string())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Value {
        Value::Str(s)
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Value {
        v.map_or(Value::Null, Into::into)
    }
}

/// Integers print exactly; everything else is rounded to 4 decimal places.
fn format_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        return format!("{}", n as i64);
    }
    let s = format!("{n:.4}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0" } else { s }.to_string()
}

fn write_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

pub fn parse(text: &str) -> Result<Value, String> {
    let mut p = Parser {
        s: text.as_bytes(),
        pos: 0,
    };
    let value = p.value()?;
    p.ws();
    if p.pos != p.s.len() {
        return Err(p.error("trailing characters"));
    }
    Ok(value)
}

struct Parser<'a> {
    s: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn error(&self, what: &str) -> String {
        format!("{what} at byte {}", self.pos)
    }

    fn ws(&mut self) {
        while self.s.get(self.pos).is_some_and(u8::is_ascii_whitespace) {
            self.pos += 1;
        }
    }

    fn eat(&mut self, byte: u8) -> bool {
        self.ws();
        let found = self.s.get(self.pos) == Some(&byte);
        if found {
            self.pos += 1;
        }
        found
    }

    fn literal(&mut self, word: &str, value: Value) -> Result<Value, String> {
        if self.s[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(value)
        } else {
            Err(self.error("unexpected character"))
        }
    }

    fn value(&mut self) -> Result<Value, String> {
        self.ws();
        match self.s.get(self.pos) {
            None => Err(self.error("unexpected end of input")),
            Some(b'n') => self.literal("null", Value::Null),
            Some(b't') => self.literal("true", Value::Bool(true)),
            Some(b'f') => self.literal("false", Value::Bool(false)),
            Some(b'"') => self.string().map(Value::Str),
            Some(b'[') => {
                self.pos += 1;
                let mut items = Vec::new();
                if !self.eat(b']') {
                    loop {
                        items.push(self.value()?);
                        if self.eat(b']') {
                            break;
                        }
                        if !self.eat(b',') {
                            return Err(self.error("expected ',' or ']'"));
                        }
                    }
                }
                Ok(Value::Arr(items))
            }
            Some(b'{') => {
                self.pos += 1;
                let mut fields = Vec::new();
                if !self.eat(b'}') {
                    loop {
                        self.ws();
                        let key = self.string()?;
                        if !self.eat(b':') {
                            return Err(self.error("expected ':'"));
                        }
                        fields.push((key, self.value()?));
                        if self.eat(b'}') {
                            break;
                        }
                        if !self.eat(b',') {
                            return Err(self.error("expected ',' or '}'"));
                        }
                    }
                }
                Ok(Value::Obj(fields))
            }
            Some(_) => {
                let start = self.pos;
                while self
                    .s
                    .get(self.pos)
                    .is_some_and(|b| b.is_ascii_digit() || b"+-.eE".contains(b))
                {
                    self.pos += 1;
                }
                std::str::from_utf8(&self.s[start..self.pos])
                    .ok()
                    .and_then(|n| n.parse().ok())
                    .map(Value::Num)
                    .ok_or_else(|| self.error("bad number"))
            }
        }
    }

    fn string(&mut self) -> Result<String, String> {
        if self.s.get(self.pos) != Some(&b'"') {
            return Err(self.error("expected a string"));
        }
        self.pos += 1;
        let mut out = Vec::new();
        loop {
            let Some(&byte) = self.s.get(self.pos) else {
                return Err(self.error("unterminated string"));
            };
            self.pos += 1;
            match byte {
                b'"' => break,
                b'\\' => {
                    let Some(&escape) = self.s.get(self.pos) else {
                        return Err(self.error("unterminated string"));
                    };
                    self.pos += 1;
                    let c = match escape {
                        b'n' => '\n',
                        b't' => '\t',
                        b'r' => '\r',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'u' => {
                            let code = self
                                .s
                                .get(self.pos..self.pos + 4)
                                .and_then(|h| std::str::from_utf8(h).ok())
                                .and_then(|h| u32::from_str_radix(h, 16).ok())
                                .ok_or_else(|| self.error("bad \\u escape"))?;
                            self.pos += 4;
                            char::from_u32(code).unwrap_or(char::REPLACEMENT_CHARACTER)
                        }
                        other => other as char,
                    };
                    out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
                }
                other => out.push(other),
            }
        }
        String::from_utf8(out).map_err(|_| self.error("invalid UTF-8"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips() {
        let value = Value::Obj(vec![
            ("name".into(), "part-01 \"wal\"\n\\".into()),
            ("n".into(), 1_000_000u64.into()),
            ("x".into(), 2.5.into()),
            ("small".into(), 0.000_01.into()),
            ("none".into(), Value::Null),
            ("ok".into(), true.into()),
            (
                "list".into(),
                Value::Arr(vec![1u64.into(), Value::Arr(vec![])]),
            ),
            ("empty".into(), Value::Obj(vec![])),
        ]);
        let text = value.to_pretty();
        let parsed = parse(&text).unwrap();
        assert_eq!(parsed.get("name"), value.get("name"));
        assert_eq!(parsed.get("n").and_then(Value::as_f64), Some(1e6));
        assert_eq!(parsed.get("small").and_then(Value::as_f64), Some(0.0));
        assert_eq!(parsed.get("list"), value.get("list"));
        assert_eq!(parse(&parsed.to_pretty()).unwrap(), parsed);
    }

    #[test]
    fn rejects_garbage() {
        for bad in ["", "{", "[1,]", "{\"a\" 1}", "nul", "\"abc", "1 2"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }
}
