// Copyright 2020-2021, The Tremor Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! The `binflux` codec is a binary representation of the influx line protocol.
//!
//! It exhibits significantly faster serialization performance, taking less space on the wire.
//!
//! The format does not include framing and SHOULD be used with `size-prefix` processors.
//!
//! For all numerics network byte order is used (big endian).
//!
//! ## Data Representation
//!
//! 1. _2 byte_ (u16) length of the `measurement` in bytes
//! 2. _n byte_ (utf8) the measurement (utf8 encoded string)
//! 3. _8 byte_ (u64) the timestamp
//! 4. _2 byte_ (u16) number of tags (key value pairs) repetitions of:
//!    1. _2 byte_ (u16) length of the tag name in bytes
//!    2. _n byte_ (utf8) tag name (utf8 encoded string)
//!    3. _2 byte_ (u16) length of tag value in bytes
//!    4. _n byte_ (utf8) tag value (utf8 encoded string)
//! 5. _2 byte_ (u16) number of fiends (key value pairs) repetition of:
//!    1. _2 byte_ (u16) length of the tag name in bytes
//!    2. _n byte_ (utf8) tag name (utf8 encoded string)
//!    3. _1 byte_ (tag) type of the field value can be one of:
//!    4. `TYPE_I64 = 0` followed by _8 byte_ (i64)
//!       1. `TYPE_F64 = 1` followed by _8 byte_ (f64)
//!       2. `TYPE_TRUE = 2` no following data
//!       3. `TYPE_FALSE = 3` no following data
//!       4. `TYPE_STRING = 4` followed by _2 byte_ (u16) length of the string in bytes and _n byte_ string value (utf8 encoded string)
//!
//! ## Origins
//!
//! The format originated with tremor as an efficient alternative to the [influx](./influx) codec.

use super::prelude::*;
use beef::Cow;
use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use simd_json::ObjectHasher;
use std::convert::TryFrom;
use std::io::{Cursor, Write};
use std::str;
use tremor_value::{literal, Object, Value};

const TYPE_I64: u8 = 0;
const TYPE_F64: u8 = 1;
const TYPE_STRING: u8 = 2;
const TYPE_TRUE: u8 = 3;
const TYPE_FALSE: u8 = 4;

#[derive(Clone, Default)]
pub struct BInflux {
    buf: Vec<u8>,
}

impl BInflux {
    pub fn encode(v: &Value, res: &mut impl Write) -> Result<()> {
        fn write_str(w: &mut impl Write, s: &str) -> Result<()> {
            w.write_u16::<BigEndian>(
                u16::try_from(s.len())
                    .chain_err(|| ErrorKind::InvalidBInfluxData("string too long".into()))?,
            )?;
            w.write_all(s.as_bytes())?;
            Ok(())
        }

        res.write_u16::<BigEndian>(0)?;
        if let Some(measurement) = v.get_str("measurement") {
            write_str(res, measurement)?;
        } else {
            return Err(ErrorKind::InvalidBInfluxData("measurement missing".into()).into());
        }

        if let Some(timestamp) = v.get_u64("timestamp") {
            res.write_u64::<BigEndian>(timestamp)?;
        } else {
            return Err(ErrorKind::InvalidBInfluxData("timestamp missing".into()).into());
        }
        if let Some(tags) = v.get_object("tags") {
            res.write_u16::<BigEndian>(
                u16::try_from(tags.len())
                    .chain_err(|| ErrorKind::InvalidBInfluxData("too many tags".into()))?,
            )?;

            for (k, v) in tags {
                if let Some(v) = v.as_str() {
                    write_str(res, k)?;
                    write_str(res, v)?;
                }
            }
        } else {
            res.write_u16::<BigEndian>(0)?;
        }

        if let Some(fields) = v.get_object("fields") {
            res.write_u16::<BigEndian>(
                u16::try_from(fields.len())
                    .chain_err(|| ErrorKind::InvalidBInfluxData("too many fields".into()))?,
            )?;
            for (k, v) in fields {
                write_str(res, k)?;
                if let Some(v) = v.as_i64() {
                    res.write_u8(TYPE_I64)?;
                    res.write_i64::<BigEndian>(v)?;
                } else if let Some(v) = v.as_f64() {
                    res.write_u8(TYPE_F64)?;
                    res.write_f64::<BigEndian>(v)?;
                } else if let Some(v) = v.as_bool() {
                    if v {
                        res.write_u8(TYPE_TRUE)?;
                    } else {
                        res.write_u8(TYPE_FALSE)?;
                    }
                } else if let Some(v) = v.as_str() {
                    res.write_u8(TYPE_STRING)?;
                    write_str(res, v)?;
                } else {
                    return Err(ErrorKind::InvalidBInfluxData(format!(
                        "Unknown type as influx line value: {:?}",
                        v.value_type()
                    ))
                    .into());
                }
            }
        } else {
            res.write_u16::<BigEndian>(0)?;
        }
        Ok(())
    }

    pub fn decode(data: &[u8]) -> Result<Value> {
        fn read_string<'event>(c: &mut Cursor<&'event [u8]>) -> Result<Cow<'event, str>> {
            let l = c.read_u16::<BigEndian>()? as usize;
            #[allow(clippy::cast_possible_truncation)]
            let p = c.position() as usize;
            c.set_position((p + l) as u64);
            unsafe { Ok(str::from_utf8_unchecked(&c.get_ref()[p..p + l]).into()) }
        }
        let mut c = Cursor::new(data);
        let vsn = c.read_u16::<BigEndian>()?;
        if vsn != 0 {
            return Err(ErrorKind::InvalidBInfluxData("invalid version".into()).into());
        };
        let measurement = read_string(&mut c)?;
        let timestamp = c.read_u64::<BigEndian>()?;
        let tag_count = c.read_u16::<BigEndian>()? as usize;
        let mut tags = Object::with_capacity_and_hasher(tag_count, ObjectHasher::default());
        for _i in 0..tag_count {
            let key = read_string(&mut c)?;
            let value = read_string(&mut c)?;
            tags.insert(key, Value::from(value));
        }
        let field_count = c.read_u16::<BigEndian>()? as usize;
        let mut fields = Object::with_capacity_and_hasher(field_count, ObjectHasher::default());
        for _i in 0..field_count {
            let key = read_string(&mut c)?;
            let kind = c.read_u8()?;
            match kind {
                TYPE_I64 => {
                    let value = c.read_i64::<BigEndian>()?;
                    fields.insert(key, Value::from(value));
                }
                TYPE_F64 => {
                    let value = c.read_f64::<BigEndian>()?;
                    fields.insert(key, Value::from(value));
                }
                TYPE_STRING => {
                    let value = read_string(&mut c)?;
                    fields.insert(key, Value::from(value));
                }
                TYPE_TRUE => {
                    fields.insert(key, Value::from(true));
                }
                TYPE_FALSE => {
                    fields.insert(key, Value::from(false));
                }
                o => error!("bad field type: {}", o),
            }
        }
        Ok(literal!({
            "measurement": measurement,
            "tags": tags,
            "fields": fields,
            "timestamp": timestamp
        }))
    }
}

impl Codec for BInflux {
    fn name(&self) -> &str {
        "binflux"
    }

    fn decode<'input>(
        &mut self,
        data: &'input mut [u8],
        _ingest_ns: u64,
    ) -> Result<Option<Value<'input>>> {
        Self::decode(data).map(Some)
    }

    fn encode(&mut self, data: &Value) -> Result<Vec<u8>> {
        Self::encode(data, &mut self.buf)?;
        let v = self.buf.clone();
        self.buf.clear();
        Ok(v)
    }

    fn boxed_clone(&self) -> Box<dyn Codec> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    #[test]
    fn errors() {
        let mut o = Value::object();
        let mut c = BInflux::default();
        assert_eq!(
            c.encode(&o)
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default(),
            "Invalid BInflux Line Protocol data: measurement missing"
        );

        o.try_insert("measurement", "m");
        assert_eq!(
            c.encode(&o)
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default(),
            "Invalid BInflux Line Protocol data: timestamp missing"
        );
        o.try_insert("timestamp", 42);
        let mut fields = Value::object();
        fields.try_insert("snot", vec![1]);

        o.try_insert("fields", fields);
        assert_eq!(
            c.encode(&o)
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default(),
            "Invalid BInflux Line Protocol data: Unknown type as influx line value: Array"
        );
    }
}
