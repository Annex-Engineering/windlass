use std::collections::BTreeMap;

use crate::{
    encoding::encode_vlq_int,
    messages::{MessageParser, MessageSkipperError},
};

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(untagged)]
pub enum ConfigVar {
    String(String),
    Number(f64),
}

#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
pub enum Enumeration {
    Single(i64),
    Range(i64, i64),
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct RawDictionary {
    #[serde(default)]
    config: BTreeMap<String, ConfigVar>,

    #[serde(default)]
    enumerations: BTreeMap<String, BTreeMap<String, Enumeration>>,

    #[serde(default)]
    commands: BTreeMap<String, i16>,
    #[serde(default)]
    responses: BTreeMap<String, i16>,
    #[serde(default)]
    output: BTreeMap<String, i16>,

    #[serde(default)]
    build_versions: Option<String>,
    #[serde(default)]
    version: Option<String>,

    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

/// Dictionary error
#[derive(thiserror::Error, Debug)]
pub enum DictionaryError {
    /// Found an empty command
    #[error("empty command found")]
    EmptyCommand,
    /// Received a command in an invalid format
    #[error("invalid command format: {0}")]
    InvalidCommandFormat(String, MessageSkipperError),
    /// Received an output string with an invalid format
    #[error("invalid output format: {0}")]
    InvalidOutputFormat(String, MessageSkipperError),
    /// Received a command with an invalid tag
    #[error("command tag {0} output valid range of -32..95")]
    InvalidCommandTag(u16),
}

#[derive(Debug)]
pub struct Dictionary {
    pub message_ids: BTreeMap<String, u16>,
    pub message_parsers: BTreeMap<u16, MessageParser>,
    pub config: BTreeMap<String, ConfigVar>,
    pub enumerations: BTreeMap<String, BTreeMap<String, Enumeration>>,
    pub build_versions: Option<String>,
    pub version: Option<String>,
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl Dictionary {
    pub(crate) fn from_raw_dictionary(raw: RawDictionary) -> Result<Self, DictionaryError> {
        let mut message_ids = BTreeMap::new();
        let mut message_parsers = BTreeMap::new();

        for (cmd, tag) in raw.commands {
            let mut split = cmd.split(' ');
            let name = split.next().ok_or(DictionaryError::EmptyCommand)?;
            let parser = MessageParser::new(name, split)
                .map_err(|e| DictionaryError::InvalidCommandFormat(name.to_string(), e))?;
            let tag = Self::map_tag(tag)?;
            message_parsers.insert(tag, parser);
            message_ids.insert(name.to_string(), tag);
        }

        for (resp, tag) in raw.responses {
            let mut split = resp.split(' ');
            let name = split.next().ok_or(DictionaryError::EmptyCommand)?;
            let parser = MessageParser::new(name, split)
                .map_err(|e| DictionaryError::InvalidCommandFormat(name.to_string(), e))?;
            let tag = Self::map_tag(tag)?;
            message_parsers.insert(tag, parser);
            message_ids.insert(name.to_string(), tag);
        }

        for (msg, tag) in raw.output {
            let parser = MessageParser::new_output(&msg)
                .map_err(|e| DictionaryError::InvalidCommandFormat(msg.to_string(), e))?;
            let tag = Self::map_tag(tag)?;
            message_parsers.insert(tag, parser);
        }

        Ok(Dictionary {
            message_ids,
            message_parsers,
            config: raw.config,
            enumerations: raw.enumerations,
            build_versions: raw.build_versions,
            version: raw.version,
            extra: raw.extra,
        })
    }

    fn map_tag(tag: i16) -> Result<u16, DictionaryError> {
        let mut buf = vec![];
        encode_vlq_int(&mut buf, tag as u32);
        let v = if buf.len() > 1 {
            ((buf[0] as u16) & 0x7F) << 7 | (buf[1] as u16) & 0x7F
        } else {
            (buf[0] as u16) & 0x7F
        };
        if v >= 1 << 14 {
            Err(DictionaryError::InvalidCommandTag(v))
        } else {
            Ok(v)
        }
    }
}
