use std::collections::BTreeMap;

use crate::dictionary::Dictionary;
use crate::encoding::{FieldType, FieldValue, MessageDecodeError};

#[derive(thiserror::Error, Debug)]
pub enum MessageSkipperError {
    #[error("invalid argument format: {0}")]
    InvalidArgumentFormat(String),
    #[error("unknown type '{1}' for argument '{0}'")]
    UnknownType(String, String),
    #[error("invalid format field type '%{0}'")]
    InvalidFormatFieldType(String),
}

pub(crate) fn format_command_args<'a>(
    fields: impl Iterator<Item = (&'a str, FieldType)>,
) -> String {
    let mut buf = String::new();
    for (idx, (name, ty)) in fields.enumerate() {
        if idx != 0 {
            buf.push(' ');
        }
        buf.push_str(name);
        buf.push('=');
        buf.push_str(match ty {
            FieldType::U32 => "%u",
            FieldType::I32 => "%i",
            FieldType::U16 => "%hu",
            FieldType::I16 => "%hi",
            FieldType::U8 => "%c",
            FieldType::String => "%s",
            FieldType::ByteArray => "%*s",
        });
    }
    buf
}

pub struct MessageParser {
    pub name: String,
    pub fields: Vec<(String, FieldType)>,
    pub output: Option<OutputFormat>,
}

impl MessageParser {
    pub(crate) fn new<'a>(
        name: &str,
        parts: impl Iterator<Item = &'a str>,
    ) -> Result<MessageParser, MessageSkipperError> {
        let mut fields = vec![];
        for part in parts {
            let (arg, ty) = part
                .split_once('=')
                .ok_or_else(|| MessageSkipperError::InvalidArgumentFormat(part.into()))?;

            let field_type = FieldType::from_msg(ty)?;
            fields.push((arg.to_string(), field_type));
        }
        Ok(Self {
            name: name.to_string(),
            fields,
            output: None,
        })
    }

    pub(crate) fn new_output(msg: &str) -> Result<MessageParser, MessageSkipperError> {
        let mut fields = vec![];
        let mut parts = vec![];

        let mut work = msg;
        while let Some(pos) = work.find('%') {
            let (pre, rest) = work.split_at(pos);
            if !pre.is_empty() {
                parts.push(FormatBlock::Static(pre.to_string()));
            }
            if let Some(rest) = rest.strip_prefix("%%") {
                parts.push(FormatBlock::Static("%".to_string()));
                work = rest;
                break;
            }
            let (format, rest) = FieldType::from_format(rest)?;
            parts.push(FormatBlock::Field);
            fields.push((format!("field_{}", fields.len()), format));
            work = rest;
        }
        if !work.is_empty() {
            parts.push(FormatBlock::Static(work.to_string()));
        }

        Ok(Self {
            name: msg.to_string(),
            fields,
            output: Some(OutputFormat { parts }),
        })
    }

    #[allow(dead_code)]
    pub(crate) fn skip(&self, input: &mut &[u8]) -> Result<(), MessageDecodeError> {
        for (_, field) in &self.fields {
            field.skip(input)?;
        }
        Ok(())
    }

    pub(crate) fn skip_with_oid(
        &self,
        input: &mut &[u8],
    ) -> Result<Option<u8>, MessageDecodeError> {
        let mut oid = None;
        for (name, field) in &self.fields {
            if name == "oid" {
                if let FieldValue::U8(read_oid) = field.read(input)? {
                    oid = Some(read_oid);
                }
            } else {
                field.skip(input)?;
            }
        }
        Ok(oid)
    }

    pub(crate) fn parse(
        &self,
        input: &mut &[u8],
    ) -> Result<BTreeMap<String, FieldValue>, MessageDecodeError> {
        let mut output = BTreeMap::new();
        for (name, field) in &self.fields {
            output.insert(name.to_string(), field.read(input)?);
        }
        Ok(output)
    }

    pub(crate) fn parse_and_output(
        &self,
        input: &mut &[u8],
    ) -> Option<Result<String, MessageDecodeError>> {
        self.output.as_ref().map(|output| {
            let fields = self.parse(input)?;
            Ok(output.format(fields.values()))
        })
    }
}

impl std::fmt::Debug for MessageParser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map()
            .entry(&"name", &self.name)
            .entry(&"fields", &self.fields)
            .finish()
    }
}

pub trait Message: 'static {
    type Pod<'a>: Into<Self::PodOwned> + std::fmt::Debug;
    type PodOwned: Clone + Send + std::fmt::Debug + 'static;
    fn get_id(dict: Option<&Dictionary>) -> Option<u16>;
    fn get_name() -> &'static str;
    fn decode<'a>(input: &mut &'a [u8]) -> Result<Self::Pod<'a>, MessageDecodeError>;
    fn fields() -> Vec<(&'static str, FieldType)>;
}

pub trait WithOid: 'static {}
pub trait WithoutOid: 'static {}

/// Represents an encoded message, with a type-level link to the message kind
pub struct EncodedMessage<M> {
    pub payload: FrontTrimmableBuffer,
    pub _message_kind: std::marker::PhantomData<M>,
}

impl<R: Message> std::fmt::Debug for EncodedMessage<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncodedMessage")
            .field("kind", &R::get_name())
            .field("payload", &self.payload)
            .finish()
    }
}

impl<M: Message> EncodedMessage<M> {
    #[doc(hidden)]
    pub fn message_id(&self, dict: Option<&Dictionary>) -> Option<u16> {
        M::get_id(dict)
    }

    #[doc(hidden)]
    pub fn message_name(&self) -> &'static str {
        M::get_name()
    }
}

/// Wraps a Vec<u8> allowing removal of front bytes in a zero-copy way
pub struct FrontTrimmableBuffer {
    pub content: Vec<u8>,
    pub offset: usize,
}

impl FrontTrimmableBuffer {
    pub fn as_slice(&self) -> &[u8] {
        &self.content[self.offset..]
    }
}

impl std::fmt::Debug for FrontTrimmableBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.as_slice(), f)
    }
}

/// Holds the format of a `output()` style debug message
#[derive(Debug)]
pub struct OutputFormat {
    parts: Vec<FormatBlock>,
}

impl OutputFormat {
    fn format<'a>(&self, mut fields: impl Iterator<Item = &'a FieldValue>) -> String {
        let mut buf = String::new();
        for part in &self.parts {
            match part {
                FormatBlock::Static(s) => buf.push_str(s),
                FormatBlock::Field => {
                    if let Some(v) = fields.next() {
                        std::fmt::write(&mut buf, format_args!("{v}")).ok();
                    }
                }
            }
        }
        buf
    }
}

#[derive(Debug)]
enum FormatBlock {
    Static(String),
    Field,
}
