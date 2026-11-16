use std::collections::BTreeMap;
use std::str;

#[derive(Debug)]
pub(crate) enum Json {
    Array(Vec<Self>),
    Object(BTreeMap<String, Self>),
    Text(String),
    Boolean,
    Scalar,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct InvalidJson;

pub(crate) fn parse(bytes: &[u8], max_depth: usize) -> Result<Json, InvalidJson> {
    let source = str::from_utf8(bytes).map_err(|_| InvalidJson)?;
    let mut parser = Parser {
        source,
        position: 0,
    };
    let value = parser.value(max_depth)?;
    parser.whitespace();
    if parser.position != source.len() {
        return Err(InvalidJson);
    }
    Ok(value)
}

struct Parser<'a> {
    source: &'a str,
    position: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.source.as_bytes().get(self.position).copied()
    }

    fn take(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn require(&mut self, byte: u8) -> Result<(), InvalidJson> {
        self.take(byte).then_some(()).ok_or(InvalidJson)
    }

    fn whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.position += 1;
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, InvalidJson> {
        self.whitespace();
        match self.peek() {
            Some(b'[') if depth > 0 => self.array(depth - 1).map(Json::Array),
            Some(b'{') if depth > 0 => self.object(depth - 1).map(Json::Object),
            Some(b'"') => self.string().map(Json::Text),
            Some(b't') => self.literal("true", Json::Boolean),
            Some(b'f') => self.literal("false", Json::Boolean),
            Some(b'n') => self.literal("null", Json::Scalar),
            Some(b'-' | b'0'..=b'9') => self.number().map(|()| Json::Scalar),
            _ => Err(InvalidJson),
        }
    }

    fn literal(&mut self, token: &str, value: Json) -> Result<Json, InvalidJson> {
        if !self.source[self.position..].starts_with(token) {
            return Err(InvalidJson);
        }
        self.position += token.len();
        Ok(value)
    }

    fn array(&mut self, depth: usize) -> Result<Vec<Json>, InvalidJson> {
        self.require(b'[')?;
        self.whitespace();
        let mut elements = Vec::new();
        if !self.take(b']') {
            loop {
                elements.push(self.value(depth)?);
                self.whitespace();
                if self.take(b']') {
                    break;
                }
                self.require(b',')?;
            }
        }
        Ok(elements)
    }

    fn object(&mut self, depth: usize) -> Result<BTreeMap<String, Json>, InvalidJson> {
        self.require(b'{')?;
        self.whitespace();
        let mut fields = BTreeMap::new();
        if !self.take(b'}') {
            loop {
                self.whitespace();
                let name = self.string()?;
                self.whitespace();
                self.require(b':')?;
                if fields.insert(name, self.value(depth)?).is_some() {
                    return Err(InvalidJson);
                }
                self.whitespace();
                if self.take(b'}') {
                    break;
                }
                self.require(b',')?;
            }
        }
        Ok(fields)
    }

    fn number(&mut self) -> Result<(), InvalidJson> {
        self.take(b'-');
        if !self.take(b'0') {
            self.digits()?;
        }
        if self.take(b'.') {
            self.digits()?;
        }
        if self.take(b'e') || self.take(b'E') {
            if !self.take(b'+') {
                self.take(b'-');
            }
            self.digits()?;
        }
        Ok(())
    }

    fn digits(&mut self) -> Result<(), InvalidJson> {
        let start = self.position;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.position += 1;
        }
        (self.position != start).then_some(()).ok_or(InvalidJson)
    }

    fn string(&mut self) -> Result<String, InvalidJson> {
        self.require(b'"')?;
        let mut output = String::new();
        loop {
            let rest = &self.source[self.position..];
            let character = rest.chars().next().ok_or(InvalidJson)?;
            self.position += character.len_utf8();
            match character {
                '"' => return Ok(output),
                '\\' => output.push(self.escape()?),
                '\u{0000}'..='\u{001f}' => return Err(InvalidJson),
                character => output.push(character),
            }
        }
    }

    fn escape(&mut self) -> Result<char, InvalidJson> {
        let byte = self.peek().ok_or(InvalidJson)?;
        self.position += 1;
        match byte {
            b'"' | b'\\' | b'/' => Ok(char::from(byte)),
            b'b' => Ok('\u{0008}'),
            b'f' => Ok('\u{000c}'),
            b'n' => Ok('\n'),
            b'r' => Ok('\r'),
            b't' => Ok('\t'),
            b'u' => {
                let high = self.code_unit()?;
                let value = if (0xd800..=0xdbff).contains(&high) {
                    self.require(b'\\')?;
                    self.require(b'u')?;
                    let low = self.code_unit()?;
                    if !(0xdc00..=0xdfff).contains(&low) {
                        return Err(InvalidJson);
                    }
                    0x1_0000 + (u32::from(high - 0xd800) << 10) + u32::from(low - 0xdc00)
                } else {
                    u32::from(high)
                };
                char::from_u32(value).ok_or(InvalidJson)
            }
            _ => Err(InvalidJson),
        }
    }

    fn code_unit(&mut self) -> Result<u16, InvalidJson> {
        let mut unit = 0;
        for _ in 0..4 {
            let digit = self
                .peek()
                .map(char::from)
                .and_then(|byte| byte.to_digit(16))
                .ok_or(InvalidJson)?;
            self.position += 1;
            unit = (unit << 4) | u16::try_from(digit).map_err(|_| InvalidJson)?;
        }
        Ok(unit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_duplicate_decoded_member_names_and_invalid_unicode() {
        for invalid in [
            br#"{"name":1,"\u006Eame":2}"#.as_slice(),
            br#""\uD800""#,
            br#""\uDC00""#,
            b"[1,]",
            b"01",
            b"1e+",
            b"true false",
            b"\"\xff\"",
        ] {
            assert!(parse(invalid, 64).is_err(), "{invalid:?}");
        }
        assert!(parse(br#"["\uD834\uDD1E",-0.1e+2,true,false,null]"#, 64).is_ok());
    }

    #[test]
    fn caps_container_depth() {
        let nested = |levels: usize| format!("{}0{}", "[".repeat(levels), "]".repeat(levels));
        assert!(parse(nested(64).as_bytes(), 64).is_ok());
        assert!(parse(nested(65).as_bytes(), 64).is_err());
    }
}
