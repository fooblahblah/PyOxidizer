// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

/*! Code requirement language primitives.

Code signatures contain a binary encoded expression tree denoting requirements.
There is a human friendly DSL that can be turned into these binary expressions
using the `csreq` Apple tool. This module reimplements that language.

# Binary Encoding

Requirement expressions consist of opcodes. An opcode is defined by a u32 where
the high byte contains flags and the lower 3 bytes denote the opcode value.

Some opcodes have payloads and the payload varies by opcode. A common pattern
is to length encode arbitrary data via a u32 denoting the length and N bytes
to follow.

String data is not guaranteed to be terminated by a NULL. However, variable
length data is padded will NULL bytes so the next opcode is always aligned
on 4 byte boundaries.

*/

use {
    crate::macho::{read_and_validate_blob_header, CodeSigningMagic},
    bcder::Oid,
    chrono::TimeZone,
    scroll::Pread,
    std::{borrow::Cow, convert::TryFrom},
};

const OPCODE_FLAG_MASK: u32 = 0xff000000;
const OPCODE_VALUE_MASK: u32 = 0x00ffffff;

/// Opcode flag meaning has size field, okay to default to false.
#[allow(unused)]
const OPCODE_FLAG_DEFAULT_FALSE: u32 = 0x80000000;

/// Opcode flag meaning has size field, skip and continue.
#[allow(unused)]
const OPCODE_FLAG_SKIP: u32 = 0x40000000;

/// An error related to a code requirement expression.
#[derive(Debug)]
pub enum CodeRequirementError {
    /// Unknown opcode encountered.
    UnknownOpCode(u32),
    /// Unknown match operator.
    UnknownMatch(u32),
    /// Error in scroll crate.
    Scroll(scroll::Error),
    /// Generic malformed error.
    Malformed(&'static str),
}

impl std::fmt::Display for CodeRequirementError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownOpCode(v) => f.write_fmt(format_args!("unknown opcode: {}", v)),
            Self::UnknownMatch(v) => f.write_fmt(format_args!("unknown match code: {}", v)),
            Self::Scroll(e) => f.write_fmt(format_args!("decoding error: {}", e)),
            Self::Malformed(s) => f.write_fmt(format_args!("malformed data: {}", s)),
        }
    }
}

impl std::error::Error for CodeRequirementError {}

impl From<scroll::Error> for CodeRequirementError {
    fn from(e: scroll::Error) -> Self {
        Self::Scroll(e)
    }
}

fn read_data(data: &[u8]) -> Result<(&[u8], &[u8]), CodeRequirementError> {
    let length = data.pread_with::<u32>(0, scroll::BE)?;
    let value = &data[4..4 + length as usize];

    // Next element is aligned on next 4 byte boundary.
    let offset = 4 + length as usize;

    let offset = match offset % 4 {
        0 => offset,
        extra => offset + 4 - extra,
    };

    let remaining = &data[offset..];

    Ok((value, remaining))
}

/// A value in a code requirement expression.
///
/// The value can be various primitive types. This type exists to make it
/// easier to work with and format values in code requirement expressions.
#[derive(Clone, Debug, PartialEq)]
pub enum CodeRequirementValue<'a> {
    String(Cow<'a, str>),
    Bytes(Cow<'a, [u8]>),
}

impl<'a> From<&'a [u8]> for CodeRequirementValue<'a> {
    fn from(value: &'a [u8]) -> Self {
        let is_ascii_printable = |c: &u8| -> bool {
            c.is_ascii_alphanumeric() || c.is_ascii_whitespace() || c.is_ascii_punctuation()
        };

        if value.iter().all(is_ascii_printable) {
            Self::String(unsafe { std::str::from_utf8_unchecked(value) }.into())
        } else {
            Self::Bytes(value.into())
        }
    }
}

impl<'a> From<&'a str> for CodeRequirementValue<'a> {
    fn from(s: &'a str) -> Self {
        Self::String(s.into())
    }
}

impl<'a> From<Cow<'a, str>> for CodeRequirementValue<'a> {
    fn from(v: Cow<'a, str>) -> Self {
        Self::String(v)
    }
}

impl<'a> std::fmt::Display for CodeRequirementValue<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::String(s) => f.write_str(s),
            Self::Bytes(data) => f.write_fmt(format_args!("{}", hex::encode(data))),
        }
    }
}

/// An opcode representing a code requirement expression.
#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(u32)]
enum RequirementOpCode {
    False = 0,
    True = 1,
    Identifier = 2,
    AnchorApple = 3,
    AnchorCertificateHash = 4,
    InfoKeyValueLegacy = 5,
    And = 6,
    Or = 7,
    CodeDirectoryHash = 8,
    Not = 9,
    InfoPlistExpression = 10,
    CertificateField = 11,
    CertificateTrusted = 12,
    AnchorTrusted = 13,
    CertificateGeneric = 14,
    AnchorAppleGeneric = 15,
    EntitlementsField = 16,
    CertificatePolicy = 17,
    NamedAnchor = 18,
    NamedCode = 19,
    Platform = 20,
    Notarized = 21,
    CertificateFieldDate = 22,
    LegacyDeveloperId = 23,
}

impl TryFrom<u32> for RequirementOpCode {
    type Error = CodeRequirementError;

    fn try_from(v: u32) -> Result<Self, Self::Error> {
        match v {
            0 => Ok(Self::False),
            1 => Ok(Self::True),
            2 => Ok(Self::Identifier),
            3 => Ok(Self::AnchorApple),
            4 => Ok(Self::AnchorCertificateHash),
            5 => Ok(Self::InfoKeyValueLegacy),
            6 => Ok(Self::And),
            7 => Ok(Self::Or),
            8 => Ok(Self::CodeDirectoryHash),
            9 => Ok(Self::Not),
            10 => Ok(Self::InfoPlistExpression),
            11 => Ok(Self::CertificateField),
            12 => Ok(Self::CertificateTrusted),
            13 => Ok(Self::AnchorTrusted),
            14 => Ok(Self::CertificateGeneric),
            15 => Ok(Self::AnchorAppleGeneric),
            16 => Ok(Self::EntitlementsField),
            17 => Ok(Self::CertificatePolicy),
            18 => Ok(Self::NamedAnchor),
            19 => Ok(Self::NamedCode),
            20 => Ok(Self::Platform),
            21 => Ok(Self::Notarized),
            22 => Ok(Self::CertificateFieldDate),
            23 => Ok(Self::LegacyDeveloperId),
            _ => Err(CodeRequirementError::UnknownOpCode(v)),
        }
    }
}

impl RequirementOpCode {
    /// Parse the payload of an opcode.
    ///
    /// On successful parse, returns an [ExpressionElement] and remaining data in
    /// the input slice.
    pub fn parse_payload<'a>(
        &self,
        data: &'a [u8],
    ) -> Result<(CodeRequirementExpression<'a>, &'a [u8]), CodeRequirementError> {
        match self {
            Self::False => Ok((CodeRequirementExpression::False, data)),
            Self::True => Ok((CodeRequirementExpression::True, data)),
            Self::Identifier => {
                let (value, data) = read_data(data)?;
                let s = std::str::from_utf8(value).map_err(|_| {
                    CodeRequirementError::Malformed("identifier value not a UTF-8 string")
                })?;

                Ok((CodeRequirementExpression::Identifier(Cow::from(s)), data))
            }
            Self::AnchorApple => Ok((CodeRequirementExpression::AnchorApple, data)),
            Self::AnchorCertificateHash => {
                let slot = data.pread_with::<i32>(0, scroll::BE)?;
                let digest_length = data.pread_with::<u32>(4, scroll::BE)?;
                let digest = &data[8..8 + digest_length as usize];

                Ok((
                    CodeRequirementExpression::AnchorCertificateHash(slot, digest.into()),
                    &data[8 + digest_length as usize..],
                ))
            }
            Self::InfoKeyValueLegacy => {
                let (key, data) = read_data(data)?;

                let key = std::str::from_utf8(key)
                    .map_err(|_| CodeRequirementError::Malformed("info key not a UTF-8 string"))?;

                let (value, data) = read_data(data)?;

                let value = std::str::from_utf8(value).map_err(|_| {
                    CodeRequirementError::Malformed("info value not a UTF-8 string")
                })?;

                Ok((
                    CodeRequirementExpression::InfoKeyValueLegacy(key.into(), value.into()),
                    data,
                ))
            }
            Self::And => {
                let (a, data) = CodeRequirementExpression::from_bytes(data)?;
                let (b, data) = CodeRequirementExpression::from_bytes(data)?;

                Ok((
                    CodeRequirementExpression::And(Box::new(a), Box::new(b)),
                    data,
                ))
            }
            Self::Or => {
                let (a, data) = CodeRequirementExpression::from_bytes(data)?;
                let (b, data) = CodeRequirementExpression::from_bytes(data)?;

                Ok((
                    CodeRequirementExpression::Or(Box::new(a), Box::new(b)),
                    data,
                ))
            }
            Self::CodeDirectoryHash => {
                let (value, data) = read_data(data)?;

                Ok((
                    CodeRequirementExpression::CodeDirectoryHash(value.into()),
                    data,
                ))
            }
            Self::Not => {
                let (expr, data) = CodeRequirementExpression::from_bytes(data)?;

                Ok((CodeRequirementExpression::Not(Box::new(expr)), data))
            }
            Self::InfoPlistExpression => {
                let (key, data) = read_data(data)?;

                let key = std::str::from_utf8(key)
                    .map_err(|_| CodeRequirementError::Malformed("key is not valid UTF-8"))?;

                let (expr, data) = CodeRequirementMatchExpression::from_bytes(data)?;

                Ok((
                    CodeRequirementExpression::InfoPlistKeyField(key.into(), expr),
                    data,
                ))
            }
            Self::CertificateField => {
                let slot = data.pread_with::<i32>(0, scroll::BE)?;

                let (field, data) = read_data(&data[4..])?;

                let field = std::str::from_utf8(field).map_err(|_| {
                    CodeRequirementError::Malformed("certificate field is not valid UTF-8")
                })?;

                let (expr, data) = CodeRequirementMatchExpression::from_bytes(data)?;

                Ok((
                    CodeRequirementExpression::CertificateField(slot, field.into(), expr),
                    data,
                ))
            }
            Self::CertificateTrusted => {
                let slot = data.pread_with::<i32>(0, scroll::BE)?;

                Ok((
                    CodeRequirementExpression::CertificateTrusted(slot),
                    &data[4..],
                ))
            }
            Self::AnchorTrusted => Ok((CodeRequirementExpression::AnchorTrusted, data)),
            Self::CertificateGeneric => {
                let slot = data.pread_with::<i32>(0, scroll::BE)?;

                let (oid, data) = read_data(&data[4..])?;

                let (expr, data) = CodeRequirementMatchExpression::from_bytes(data)?;

                Ok((
                    CodeRequirementExpression::CertificateGeneric(slot, Oid(oid), expr),
                    data,
                ))
            }
            Self::AnchorAppleGeneric => Ok((CodeRequirementExpression::AnchorAppleGeneric, data)),
            Self::EntitlementsField => {
                let (key, data) = read_data(data)?;

                let key = std::str::from_utf8(key)
                    .map_err(|_| CodeRequirementError::Malformed("entitlement key is not UTF-8"))?;

                let (expr, data) = CodeRequirementMatchExpression::from_bytes(data)?;

                Ok((
                    CodeRequirementExpression::EntitlementsKey(key.into(), expr),
                    data,
                ))
            }
            Self::CertificatePolicy => {
                let slot = data.pread_with::<i32>(0, scroll::BE)?;

                let (oid, data) = read_data(&data[4..])?;

                let (expr, data) = CodeRequirementMatchExpression::from_bytes(data)?;

                Ok((
                    CodeRequirementExpression::CertificatePolicy(slot, Oid(oid), expr),
                    data,
                ))
            }
            Self::NamedAnchor => {
                let (name, data) = read_data(data)?;

                let name = std::str::from_utf8(name)
                    .map_err(|_| CodeRequirementError::Malformed("named anchor isn't UTF-8"))?;

                Ok((CodeRequirementExpression::NamedAnchor(name.into()), data))
            }
            Self::NamedCode => {
                let (name, data) = read_data(data)?;

                let name = std::str::from_utf8(name)
                    .map_err(|_| CodeRequirementError::Malformed("named code isn't UTF-8"))?;

                Ok((CodeRequirementExpression::NamedCode(name.into()), data))
            }
            Self::Platform => {
                let value = data.pread_with::<u32>(0, scroll::BE)?;

                Ok((CodeRequirementExpression::Platform(value), &data[4..]))
            }
            Self::Notarized => Ok((CodeRequirementExpression::Notarized, data)),
            Self::CertificateFieldDate => {
                let slot = data.pread_with::<i32>(0, scroll::BE)?;

                let (oid, data) = read_data(&data[4..])?;

                let (expr, data) = CodeRequirementMatchExpression::from_bytes(data)?;

                Ok((
                    CodeRequirementExpression::CertificateFieldDate(slot, Oid(oid), expr),
                    data,
                ))
            }
            Self::LegacyDeveloperId => Ok((CodeRequirementExpression::LegacyDeveloperId, data)),
        }
    }
}

/// Defines a code requirement expression.
#[derive(Clone, Debug, PartialEq)]
pub enum CodeRequirementExpression<'a> {
    /// False
    ///
    /// `false`
    ///
    /// No payload.
    False,

    /// True
    ///
    /// `true`
    ///
    /// No payload.
    True,

    /// Signing identifier.
    ///
    /// `identifier <string>`
    ///
    /// 4 bytes length followed by C string.
    Identifier(Cow<'a, str>),

    /// The certificate chain must lead to an Apple root.
    ///
    /// `anchor apple`
    ///
    /// No payload.
    AnchorApple,

    /// The certificate chain must anchor to a certificate with specified SHA-1 hash.
    ///
    /// `anchor <slot> H"<hash>"`
    ///
    /// 4 bytes slot number, 4 bytes hash length, hash value.
    AnchorCertificateHash(i32, Cow<'a, [u8]>),

    /// Info.plist key value (legacy).
    ///
    /// `info[<key>] = <value>`
    ///
    /// 2 pairs of (length + value).
    InfoKeyValueLegacy(Cow<'a, str>, Cow<'a, str>),

    /// Logical and.
    ///
    /// `expr0 and expr1`
    ///
    /// Payload consists of 2 sub-expressions with no additional encoding.
    And(
        Box<CodeRequirementExpression<'a>>,
        Box<CodeRequirementExpression<'a>>,
    ),

    /// Logical or.
    ///
    /// `expr0 or expr1`
    ///
    /// Payload consists of 2 sub-expressions with no additional encoding.
    Or(
        Box<CodeRequirementExpression<'a>>,
        Box<CodeRequirementExpression<'a>>,
    ),

    /// Code directory hash.
    ///
    /// `cdhash H"<hash>"
    ///
    /// 4 bytes length followed by raw digest value.
    CodeDirectoryHash(Cow<'a, [u8]>),

    /// Logical not.
    ///
    /// `!expr`
    ///
    /// Payload is 1 sub-expression.
    Not(Box<CodeRequirementExpression<'a>>),

    /// Info plist key field.
    ///
    /// `info [key] match expression`
    ///
    /// e.g. `info [CFBundleName] exists`
    ///
    /// 4 bytes key length, key string, then match expression.
    InfoPlistKeyField(Cow<'a, str>, CodeRequirementMatchExpression<'a>),

    /// Certificate field matches.
    ///
    /// `certificate <slot> [<field>] match expression`
    ///
    /// Slot i32, 4 bytes field length, field string, then match expression.
    CertificateField(i32, Cow<'a, str>, CodeRequirementMatchExpression<'a>),

    /// Certificate in position is trusted for code signing.
    ///
    /// `certificate <position> trusted`
    ///
    /// 4 bytes certificate position.
    CertificateTrusted(i32),

    /// The certificate chain must lead to a trusted root.
    ///
    /// `anchor trusted`
    ///
    /// No payload.
    AnchorTrusted,

    /// Certificate field matches by OID.
    ///
    /// `certificate <slot> [field.<oid>] match expression`
    ///
    /// Slot i32, 4 bytes OID length, OID raw bytes, match expression.
    CertificateGeneric(i32, Oid<&'a [u8]>, CodeRequirementMatchExpression<'a>),

    /// For code signed by Apple, including from code signing certificates issued by Apple.
    ///
    /// `anchor apple generic`
    ///
    /// No payload.
    AnchorAppleGeneric,

    /// Value associated with specified key in signature's embedded entitlements dictionary.
    ///
    /// `entitlement [<key>] match expression`
    ///
    /// 4 bytes key length, key bytes, match expression.
    EntitlementsKey(Cow<'a, str>, CodeRequirementMatchExpression<'a>),

    /// OID associated with certificate in a given slot.
    ///
    /// It is unknown what the OID means.
    ///
    /// `certificate <slot> [policy.<oid>] match expression`
    CertificatePolicy(i32, Oid<&'a [u8]>, CodeRequirementMatchExpression<'a>),

    /// A named Apple anchor.
    ///
    /// `anchor apple <name>`
    ///
    /// 4 bytes name length, name bytes.
    NamedAnchor(Cow<'a, str>),

    /// Named code.
    ///
    /// `(<name>)`
    ///
    /// 4 bytes name length, name bytes.
    NamedCode(Cow<'a, str>),

    /// Platform value.
    ///
    /// `platform = <value>`
    ///
    /// Payload is a u32.
    Platform(u32),

    /// Binary is notarized.
    ///
    /// `notarized`
    ///
    /// No Payload.
    Notarized,

    /// Certificate field date.
    ///
    /// Unknown what the OID corresponds to.
    ///
    /// `certificate <slot> [timestamp.<oid>] match expression`
    CertificateFieldDate(i32, Oid<&'a [u8]>, CodeRequirementMatchExpression<'a>),

    /// Legacy developer ID used.
    LegacyDeveloperId,
}

impl<'a> std::fmt::Display for CodeRequirementExpression<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::False => f.write_str("never"),
            Self::True => f.write_str("always"),
            Self::Identifier(value) => f.write_fmt(format_args!("identifier {}", value)),
            Self::AnchorApple => f.write_str("anchor apple"),
            Self::AnchorCertificateHash(slot, digest) => {
                f.write_fmt(format_args!("anchor {} H\"{}\"", slot, hex::encode(digest)))
            }
            Self::InfoKeyValueLegacy(key, value) => {
                f.write_fmt(format_args!("info[{}] = \"{}\"", key, value))
            }
            Self::And(a, b) => f.write_fmt(format_args!("({}) and ({})", a, b)),
            Self::Or(a, b) => f.write_fmt(format_args!("({}) or ({})", a, b)),
            Self::CodeDirectoryHash(digest) => {
                f.write_fmt(format_args!("cdhash H\"{}\"", hex::encode(digest)))
            }
            Self::Not(expr) => f.write_fmt(format_args!("!({})", expr)),
            Self::InfoPlistKeyField(key, expr) => {
                f.write_fmt(format_args!("info [{}] {}", key, expr))
            }
            Self::CertificateField(slot, field, expr) => {
                f.write_fmt(format_args!("certificate {} [{}] {}", slot, field, expr))
            }
            Self::CertificateTrusted(slot) => {
                f.write_fmt(format_args!("certificate {} trusted", slot))
            }
            Self::AnchorTrusted => f.write_str("anchor trusted"),
            Self::CertificateGeneric(slot, oid, expr) => f.write_fmt(format_args!(
                "certificate {} [field.{}] {}",
                slot, oid, expr
            )),
            Self::AnchorAppleGeneric => f.write_str("anchor apple generic"),
            Self::EntitlementsKey(key, expr) => {
                f.write_fmt(format_args!("entitlement [{}] {}", key, expr))
            }
            Self::CertificatePolicy(slot, oid, expr) => f.write_fmt(format_args!(
                "certificate {} [policy.{}] {}",
                slot, oid, expr
            )),
            Self::NamedAnchor(name) => f.write_fmt(format_args!("anchor apple {}", name)),
            Self::NamedCode(name) => f.write_fmt(format_args!("({})", name)),
            Self::Platform(platform) => f.write_fmt(format_args!("platform = {}", platform)),
            Self::Notarized => f.write_str("notarized"),
            Self::CertificateFieldDate(slot, oid, expr) => f.write_fmt(format_args!(
                "certificate {} [timestamp.{}] {}",
                slot, oid, expr
            )),
            Self::LegacyDeveloperId => f.write_str("legacy"),
        }
    }
}

impl<'a> CodeRequirementExpression<'a> {
    /// Construct an expression element by reading from a slice.
    ///
    /// Returns the newly constructed element and remaining data in the slice.
    pub fn from_bytes(data: &'a [u8]) -> Result<(Self, &'a [u8]), CodeRequirementError> {
        let opcode_raw = data.pread_with::<u32>(0, scroll::BE)?;

        let _flags = opcode_raw & OPCODE_FLAG_MASK;
        let opcode = opcode_raw & OPCODE_VALUE_MASK;

        let data = &data[4..];

        let opcode = RequirementOpCode::try_from(opcode)?;

        opcode.parse_payload(data)
    }
}

/// A code requirement match expression type.
#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(u32)]
enum MatchType {
    Exists = 0,
    Equal = 1,
    Contains = 2,
    BeginsWith = 3,
    EndsWith = 4,
    LessThan = 5,
    GreaterThan = 6,
    LessThanEqual = 7,
    GreaterThanEqual = 8,
    On = 9,
    Before = 10,
    After = 11,
    OnOrBefore = 12,
    OnOrAfter = 13,
    Absent = 14,
}

impl TryFrom<u32> for MatchType {
    type Error = CodeRequirementError;

    fn try_from(v: u32) -> Result<Self, Self::Error> {
        match v {
            0 => Ok(Self::Exists),
            1 => Ok(Self::Equal),
            2 => Ok(Self::Contains),
            3 => Ok(Self::BeginsWith),
            4 => Ok(Self::EndsWith),
            5 => Ok(Self::LessThan),
            6 => Ok(Self::GreaterThan),
            7 => Ok(Self::LessThanEqual),
            8 => Ok(Self::GreaterThanEqual),
            9 => Ok(Self::On),
            10 => Ok(Self::Before),
            11 => Ok(Self::After),
            12 => Ok(Self::OnOrBefore),
            13 => Ok(Self::OnOrAfter),
            14 => Ok(Self::Absent),
            _ => Err(CodeRequirementError::UnknownMatch(v)),
        }
    }
}

impl MatchType {
    /// Parse the payload of a match expression.
    pub fn parse_payload<'a>(
        &self,
        data: &'a [u8],
    ) -> Result<(CodeRequirementMatchExpression<'a>, &'a [u8]), CodeRequirementError> {
        match self {
            Self::Exists => Ok((CodeRequirementMatchExpression::Exists, data)),
            Self::Equal => {
                let (value, data) = read_data(data)?;

                Ok((CodeRequirementMatchExpression::Equal(value.into()), data))
            }
            Self::Contains => {
                let (value, data) = read_data(data)?;

                Ok((CodeRequirementMatchExpression::Contains(value.into()), data))
            }
            Self::BeginsWith => {
                let (value, data) = read_data(data)?;

                Ok((
                    CodeRequirementMatchExpression::BeginsWith(value.into()),
                    data,
                ))
            }
            Self::EndsWith => {
                let (value, data) = read_data(data)?;

                Ok((CodeRequirementMatchExpression::EndsWith(value.into()), data))
            }
            Self::LessThan => {
                let (value, data) = read_data(data)?;

                Ok((CodeRequirementMatchExpression::LessThan(value.into()), data))
            }
            Self::GreaterThan => {
                let (value, data) = read_data(data)?;

                Ok((
                    CodeRequirementMatchExpression::GreaterThan(value.into()),
                    data,
                ))
            }
            Self::LessThanEqual => {
                let (value, data) = read_data(data)?;

                Ok((
                    CodeRequirementMatchExpression::LessThanEqual(value.into()),
                    data,
                ))
            }
            Self::GreaterThanEqual => {
                let (value, data) = read_data(data)?;

                Ok((
                    CodeRequirementMatchExpression::GreaterThanEqual(value.into()),
                    data,
                ))
            }
            Self::On => {
                let value = data.pread_with::<i64>(0, scroll::BE)?;

                Ok((
                    CodeRequirementMatchExpression::On(chrono::Utc.timestamp(value, 0)),
                    &data[8..],
                ))
            }
            Self::Before => {
                let value = data.pread_with::<i64>(0, scroll::BE)?;

                Ok((
                    CodeRequirementMatchExpression::Before(chrono::Utc.timestamp(value, 0)),
                    &data[8..],
                ))
            }
            Self::After => {
                let value = data.pread_with::<i64>(0, scroll::BE)?;

                Ok((
                    CodeRequirementMatchExpression::After(chrono::Utc.timestamp(value, 0)),
                    &data[8..],
                ))
            }
            Self::OnOrBefore => {
                let value = data.pread_with::<i64>(0, scroll::BE)?;

                Ok((
                    CodeRequirementMatchExpression::OnOrBefore(chrono::Utc.timestamp(value, 0)),
                    &data[8..],
                ))
            }
            Self::OnOrAfter => {
                let value = data.pread_with::<i64>(0, scroll::BE)?;

                Ok((
                    CodeRequirementMatchExpression::OnOrAfter(chrono::Utc.timestamp(value, 0)),
                    &data[8..],
                ))
            }
            Self::Absent => Ok((CodeRequirementMatchExpression::Absent, data)),
        }
    }
}

/// An instance of a match expression in a [CodeRequirementExpression].
#[derive(Clone, Debug, PartialEq)]
pub enum CodeRequirementMatchExpression<'a> {
    /// Entity exists.
    ///
    /// `exists`
    ///
    /// No payload.
    Exists,

    /// Equality.
    ///
    /// `= <value>`
    ///
    /// 4 bytes length, raw data.
    Equal(CodeRequirementValue<'a>),

    /// Contains.
    ///
    /// `~ <value>`
    ///
    /// 4 bytes length, raw data.
    Contains(CodeRequirementValue<'a>),

    /// Begins with.
    ///
    /// `= <value>*`
    ///
    /// 4 bytes length, raw data.
    BeginsWith(CodeRequirementValue<'a>),

    /// Ends with.
    ///
    /// `= *<value>`
    ///
    /// 4 bytes length, raw data.
    EndsWith(CodeRequirementValue<'a>),

    /// Less than.
    ///
    /// `< <value>`
    ///
    /// 4 bytes length, raw data.
    LessThan(CodeRequirementValue<'a>),

    /// Greater than.
    ///
    /// `> <value>`
    GreaterThan(CodeRequirementValue<'a>),

    /// Less than or equal to.
    ///
    /// `<= <value>`
    ///
    /// 4 bytes length, raw data.
    LessThanEqual(CodeRequirementValue<'a>),

    /// Greater than or equal to.
    ///
    /// `>= <value>`
    ///
    /// 4 bytes length, raw data.
    GreaterThanEqual(CodeRequirementValue<'a>),

    /// Timestamp value equivalent.
    ///
    /// `= timestamp "<timestamp>"`
    On(chrono::DateTime<chrono::Utc>),

    /// Timestamp value before.
    ///
    /// `< timestamp "<timestamp>"`
    Before(chrono::DateTime<chrono::Utc>),

    /// Timestamp value after.
    ///
    /// `> timestamp "<timestamp>"`
    After(chrono::DateTime<chrono::Utc>),

    /// Timestamp value equivalent or before.
    ///
    /// `<= timestamp "<timestamp>"`
    OnOrBefore(chrono::DateTime<chrono::Utc>),

    /// Timestamp value equivalent or after.
    ///
    /// `>= timestamp "<timestamp>"`
    OnOrAfter(chrono::DateTime<chrono::Utc>),

    /// Value is absent.
    ///
    /// `<empty>`
    ///
    /// No payload.
    Absent,
}

impl<'a> std::fmt::Display for CodeRequirementMatchExpression<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exists => f.write_str("/* exists */"),
            Self::Equal(value) => f.write_fmt(format_args!("= \"{}\"", value)),
            Self::Contains(value) => f.write_fmt(format_args!("~ \"{}\"", value)),
            Self::BeginsWith(value) => f.write_fmt(format_args!("= \"{}*\"", value)),
            Self::EndsWith(value) => f.write_fmt(format_args!("= \"*{}\"", value)),
            Self::LessThan(value) => f.write_fmt(format_args!("< \"{}\"", value)),
            Self::GreaterThan(value) => f.write_fmt(format_args!("> \"{}\"", value)),
            Self::LessThanEqual(value) => f.write_fmt(format_args!("<= \"{}\"", value)),
            Self::GreaterThanEqual(value) => f.write_fmt(format_args!(">= \"{}\"", value)),
            Self::On(value) => f.write_fmt(format_args!("= \"{}\"", value)),
            Self::Before(value) => f.write_fmt(format_args!("< \"{}\"", value)),
            Self::After(value) => f.write_fmt(format_args!("> \"{}\"", value)),
            Self::OnOrBefore(value) => f.write_fmt(format_args!("<= \"{}\"", value)),
            Self::OnOrAfter(value) => f.write_fmt(format_args!(">= \"{}\"", value)),
            Self::Absent => f.write_str("absent"),
        }
    }
}

impl<'a> CodeRequirementMatchExpression<'a> {
    /// Parse a match expression from bytes.
    ///
    /// The slice should begin with the match type u32.
    pub fn from_bytes(data: &'a [u8]) -> Result<(Self, &'a [u8]), CodeRequirementError> {
        let typ = data.pread_with::<u32>(0, scroll::BE)?;

        let typ = MatchType::try_from(typ)?;

        typ.parse_payload(&data[4..])
    }
}

/// Parse the binary serialization of code requirements.
///
/// This parses the data that follows the requirement blob header/magic that
/// usually accompanies the binary representation of code requirements.
pub fn parse_code_requirements(
    data: &[u8],
) -> Result<(Vec<CodeRequirementExpression>, &[u8]), CodeRequirementError> {
    let count = data.pread_with::<u32>(0, scroll::BE)?;
    let mut data = &data[4..];

    let mut elements = Vec::with_capacity(count as usize);

    for _ in 0..count {
        let res = CodeRequirementExpression::from_bytes(data)?;

        elements.push(res.0);
        data = res.1;
    }

    Ok((elements, data))
}

/// Parse a code requirement blob, which begins with header magic.
///
/// This can be used to parse the output generated by `csreq -b`.
pub fn parse_code_requirement_blob(
    data: &[u8],
) -> Result<(Vec<CodeRequirementExpression>, &[u8]), CodeRequirementError> {
    let data = read_and_validate_blob_header(data, u32::from(CodeSigningMagic::Requirement))
        .map_err(|_| CodeRequirementError::Malformed("malformed blob header"))?;

    parse_code_requirements(data)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn parse_false() {
        let source = hex::decode("0000000100000000").unwrap();

        let (els, data) = parse_code_requirements(&source).unwrap();

        assert_eq!(els, vec![CodeRequirementExpression::False]);
        assert!(data.is_empty());
    }

    #[test]
    fn parse_true() {
        let source = hex::decode("0000000100000001").unwrap();

        let (els, data) = parse_code_requirements(&source).unwrap();

        assert_eq!(els, vec![CodeRequirementExpression::True]);
        assert!(data.is_empty());
    }

    #[test]
    fn parse_identifier() {
        let source = hex::decode("000000010000000200000007666f6f2e62617200").unwrap();

        let (els, data) = parse_code_requirements(&source).unwrap();

        assert_eq!(
            els,
            vec![CodeRequirementExpression::Identifier("foo.bar".into())]
        );
        assert!(data.is_empty());
    }

    #[test]
    fn parse_anchor_apple() {
        let source = hex::decode("0000000100000003").unwrap();

        let (els, data) = parse_code_requirements(&source).unwrap();

        assert_eq!(els, vec![CodeRequirementExpression::AnchorApple]);
        assert!(data.is_empty());
    }

    #[test]
    fn parse_anchor_certificate_hash() {
        let source =
            hex::decode("0000000100000004ffffffff00000014deadbeefdeadbeefdeadbeefdeadbeefdeadbeef")
                .unwrap();

        let (els, data) = parse_code_requirements(&source).unwrap();

        assert_eq!(
            els,
            vec![CodeRequirementExpression::AnchorCertificateHash(
                -1,
                hex::decode("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef")
                    .unwrap()
                    .into()
            )]
        );
        assert!(data.is_empty());
    }

    #[test]
    fn parse_and() {
        let source = hex::decode("00000001000000060000000100000000").unwrap();

        let (els, data) = parse_code_requirements(&source).unwrap();

        assert_eq!(
            els,
            vec![CodeRequirementExpression::And(
                Box::new(CodeRequirementExpression::True),
                Box::new(CodeRequirementExpression::False)
            )]
        );
        assert!(data.is_empty());
    }

    #[test]
    fn parse_or() {
        let source = hex::decode("00000001000000070000000100000000").unwrap();

        let (els, data) = parse_code_requirements(&source).unwrap();

        assert_eq!(
            els,
            vec![CodeRequirementExpression::Or(
                Box::new(CodeRequirementExpression::True),
                Box::new(CodeRequirementExpression::False)
            )]
        );
        assert!(data.is_empty());
    }

    #[test]
    fn parse_code_directory_hash() {
        let source =
            hex::decode("000000010000000800000014deadbeefdeadbeefdeadbeefdeadbeefdeadbeef")
                .unwrap();

        let (els, data) = parse_code_requirements(&source).unwrap();

        assert_eq!(
            els,
            vec![CodeRequirementExpression::CodeDirectoryHash(
                hex::decode("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef")
                    .unwrap()
                    .into()
            )]
        );
        assert!(data.is_empty());
    }

    #[test]
    fn parse_not() {
        let source = hex::decode("000000010000000900000001").unwrap();

        let (els, data) = parse_code_requirements(&source).unwrap();

        assert_eq!(
            els,
            vec![CodeRequirementExpression::Not(Box::new(
                CodeRequirementExpression::True
            ))]
        );
        assert!(data.is_empty());
    }

    #[test]
    fn parse_info_plist_key_field() {
        let source = hex::decode("000000010000000a000000036b65790000000000").unwrap();

        let (els, data) = parse_code_requirements(&source).unwrap();

        assert_eq!(
            els,
            vec![CodeRequirementExpression::InfoPlistKeyField(
                "key".into(),
                CodeRequirementMatchExpression::Exists
            )]
        );
        assert!(data.is_empty());
    }

    #[test]
    fn parse_certificate_field() {
        let source =
            hex::decode("000000010000000bffffffff0000000a7375626a6563742e434e000000000000")
                .unwrap();

        let (els, data) = parse_code_requirements(&source).unwrap();

        assert_eq!(
            els,
            vec![CodeRequirementExpression::CertificateField(
                -1,
                "subject.CN".into(),
                CodeRequirementMatchExpression::Exists
            )]
        );
        assert!(data.is_empty());
    }

    #[test]
    fn parse_certificate_trusted() {
        let source = hex::decode("000000010000000cffffffff").unwrap();

        let (els, data) = parse_code_requirements(&source).unwrap();

        assert_eq!(els, vec![CodeRequirementExpression::CertificateTrusted(-1)]);
        assert!(data.is_empty());
    }

    #[test]
    fn parse_anchor_trusted() {
        let source = hex::decode("000000010000000d").unwrap();

        let (els, data) = parse_code_requirements(&source).unwrap();

        assert_eq!(els, vec![CodeRequirementExpression::AnchorTrusted]);
        assert!(data.is_empty());
    }

    #[test]
    fn parse_certificate_generic() {
        let source = hex::decode("000000010000000effffffff000000035504030000000000").unwrap();

        let (els, data) = parse_code_requirements(&source).unwrap();

        assert_eq!(
            els,
            vec![CodeRequirementExpression::CertificateGeneric(
                -1,
                Oid(&[0x55, 4, 3]),
                CodeRequirementMatchExpression::Exists
            )]
        );
        assert!(data.is_empty());
    }

    #[test]
    fn parse_anchor_apple_generic() {
        let source = hex::decode("000000010000000f").unwrap();

        let (els, data) = parse_code_requirements(&source).unwrap();

        assert_eq!(els, vec![CodeRequirementExpression::AnchorAppleGeneric]);
        assert!(data.is_empty());
    }

    #[test]
    fn parse_entitlements_key() {
        let source = hex::decode("0000000100000010000000036b65790000000000").unwrap();

        let (els, data) = parse_code_requirements(&source).unwrap();

        assert_eq!(
            els,
            vec![CodeRequirementExpression::EntitlementsKey(
                "key".into(),
                CodeRequirementMatchExpression::Exists
            )]
        );
        assert!(data.is_empty());
    }

    #[test]
    fn parse_certificate_policy() {
        let source = hex::decode("0000000100000011ffffffff000000035504030000000000").unwrap();

        let (els, data) = parse_code_requirements(&source).unwrap();

        assert_eq!(
            els,
            vec![CodeRequirementExpression::CertificatePolicy(
                -1,
                Oid(&[0x55, 4, 3]),
                CodeRequirementMatchExpression::Exists
            )]
        );
        assert!(data.is_empty());
    }

    #[test]
    fn parse_named_anchor() {
        let source = hex::decode("000000010000001200000003666f6f00").unwrap();

        let (els, data) = parse_code_requirements(&source).unwrap();

        assert_eq!(
            els,
            vec![CodeRequirementExpression::NamedAnchor("foo".into())]
        );
        assert!(data.is_empty());
    }

    #[test]
    fn parse_named_code() {
        let source = hex::decode("000000010000001300000003666f6f00").unwrap();

        let (els, data) = parse_code_requirements(&source).unwrap();

        assert_eq!(
            els,
            vec![CodeRequirementExpression::NamedCode("foo".into())]
        );
        assert!(data.is_empty());
    }

    #[test]
    fn parse_platform() {
        let source = hex::decode("00000001000000140000000a").unwrap();

        let (els, data) = parse_code_requirements(&source).unwrap();

        assert_eq!(els, vec![CodeRequirementExpression::Platform(10)]);
        assert!(data.is_empty());
    }

    #[test]
    fn parse_notarized() {
        let source = hex::decode("0000000100000015").unwrap();

        let (els, data) = parse_code_requirements(&source).unwrap();

        assert_eq!(els, vec![CodeRequirementExpression::Notarized]);
        assert!(data.is_empty());
    }

    #[test]
    fn parse_certificate_field_date() {
        let source = hex::decode("0000000100000016ffffffff000000035504030000000000").unwrap();

        let (els, data) = parse_code_requirements(&source).unwrap();

        assert_eq!(
            els,
            vec![CodeRequirementExpression::CertificateFieldDate(
                -1,
                Oid(&[0x55, 4, 3]),
                CodeRequirementMatchExpression::Exists,
            )]
        );
        assert!(data.is_empty());
    }

    #[test]
    fn parse_legacy() {
        let source = hex::decode("0000000100000017").unwrap();

        let (els, data) = parse_code_requirements(&source).unwrap();

        assert_eq!(els, vec![CodeRequirementExpression::LegacyDeveloperId]);
        assert!(data.is_empty());
    }

    #[test]
    fn parse_blob() {
        let source = hex::decode("fade0c00000000100000000100000000").unwrap();

        let (els, data) = parse_code_requirement_blob(&source).unwrap();

        assert_eq!(els, vec![CodeRequirementExpression::False]);
        assert!(data.is_empty());
    }

    #[test]
    fn parse_match_exists() {
        let source = hex::decode("000000010000000a000000036b65790000000000").unwrap();

        let (els, data) = parse_code_requirements(&source).unwrap();

        assert_eq!(
            els,
            vec![CodeRequirementExpression::InfoPlistKeyField(
                "key".into(),
                CodeRequirementMatchExpression::Exists
            )]
        );
        assert!(data.is_empty());
    }

    #[test]
    fn parse_match_absent() {
        let source = hex::decode("000000010000000a000000036b6579000000000e").unwrap();

        let (els, data) = parse_code_requirements(&source).unwrap();

        assert_eq!(
            els,
            vec![CodeRequirementExpression::InfoPlistKeyField(
                "key".into(),
                CodeRequirementMatchExpression::Absent
            )]
        );
        assert!(data.is_empty());
    }

    #[test]
    fn parse_match_equal() {
        let source =
            hex::decode("000000010000000a000000036b657900000000010000000576616c7565000000")
                .unwrap();

        let (els, data) = parse_code_requirements(&source).unwrap();

        assert_eq!(
            els,
            vec![CodeRequirementExpression::InfoPlistKeyField(
                "key".into(),
                CodeRequirementMatchExpression::Equal(b"value".as_ref().into())
            )]
        );
        assert!(data.is_empty());
    }

    #[test]
    fn parse_match_contains() {
        let source =
            hex::decode("000000010000000a000000036b657900000000020000000576616c7565000000")
                .unwrap();

        let (els, data) = parse_code_requirements(&source).unwrap();

        assert_eq!(
            els,
            vec![CodeRequirementExpression::InfoPlistKeyField(
                "key".into(),
                CodeRequirementMatchExpression::Contains(b"value".as_ref().into())
            )]
        );
        assert!(data.is_empty());
    }

    #[test]
    fn parse_match_begins_with() {
        let source =
            hex::decode("000000010000000a000000036b657900000000030000000576616c7565000000")
                .unwrap();

        let (els, data) = parse_code_requirements(&source).unwrap();

        assert_eq!(
            els,
            vec![CodeRequirementExpression::InfoPlistKeyField(
                "key".into(),
                CodeRequirementMatchExpression::BeginsWith(b"value".as_ref().into())
            )]
        );
        assert!(data.is_empty());
    }

    #[test]
    fn parse_match_ends_with() {
        let source =
            hex::decode("000000010000000a000000036b657900000000040000000576616c7565000000")
                .unwrap();

        let (els, data) = parse_code_requirements(&source).unwrap();

        assert_eq!(
            els,
            vec![CodeRequirementExpression::InfoPlistKeyField(
                "key".into(),
                CodeRequirementMatchExpression::EndsWith(b"value".as_ref().into())
            )]
        );
        assert!(data.is_empty());
    }

    #[test]
    fn parse_match_less_than() {
        let source =
            hex::decode("000000010000000a000000036b657900000000050000000576616c7565000000")
                .unwrap();

        let (els, data) = parse_code_requirements(&source).unwrap();

        assert_eq!(
            els,
            vec![CodeRequirementExpression::InfoPlistKeyField(
                "key".into(),
                CodeRequirementMatchExpression::LessThan(b"value".as_ref().into())
            )]
        );
        assert!(data.is_empty());
    }

    #[test]
    fn parse_match_greater_than() {
        let source =
            hex::decode("000000010000000a000000036b657900000000060000000576616c7565000000")
                .unwrap();

        let (els, data) = parse_code_requirements(&source).unwrap();

        assert_eq!(
            els,
            vec![CodeRequirementExpression::InfoPlistKeyField(
                "key".into(),
                CodeRequirementMatchExpression::GreaterThan(b"value".as_ref().into())
            )]
        );
        assert!(data.is_empty());
    }

    #[test]
    fn parse_match_less_than_equal() {
        let source =
            hex::decode("000000010000000a000000036b657900000000070000000576616c7565000000")
                .unwrap();

        let (els, data) = parse_code_requirements(&source).unwrap();

        assert_eq!(
            els,
            vec![CodeRequirementExpression::InfoPlistKeyField(
                "key".into(),
                CodeRequirementMatchExpression::LessThanEqual(b"value".as_ref().into())
            )]
        );
        assert!(data.is_empty());
    }

    #[test]
    fn parse_match_greater_than_equal() {
        let source =
            hex::decode("000000010000000a000000036b657900000000080000000576616c7565000000")
                .unwrap();

        let (els, data) = parse_code_requirements(&source).unwrap();

        assert_eq!(
            els,
            vec![CodeRequirementExpression::InfoPlistKeyField(
                "key".into(),
                CodeRequirementMatchExpression::GreaterThanEqual(b"value".as_ref().into())
            )]
        );
        assert!(data.is_empty());
    }

    #[test]
    fn parse_match_on() {
        let source =
            hex::decode("000000010000000a000000036b6579000000000900000000605fca30").unwrap();

        let (els, data) = parse_code_requirements(&source).unwrap();

        assert_eq!(
            els,
            vec![CodeRequirementExpression::InfoPlistKeyField(
                "key".into(),
                CodeRequirementMatchExpression::On(chrono::Utc.timestamp(1616890416, 0)),
            )]
        );
        assert!(data.is_empty());
    }

    #[test]
    fn parse_match_before() {
        let source =
            hex::decode("000000010000000a000000036b6579000000000a00000000605fca30").unwrap();

        let (els, data) = parse_code_requirements(&source).unwrap();

        assert_eq!(
            els,
            vec![CodeRequirementExpression::InfoPlistKeyField(
                "key".into(),
                CodeRequirementMatchExpression::Before(chrono::Utc.timestamp(1616890416, 0)),
            )]
        );
        assert!(data.is_empty());
    }

    #[test]
    fn parse_match_after() {
        let source =
            hex::decode("000000010000000a000000036b6579000000000b00000000605fca30").unwrap();

        let (els, data) = parse_code_requirements(&source).unwrap();

        assert_eq!(
            els,
            vec![CodeRequirementExpression::InfoPlistKeyField(
                "key".into(),
                CodeRequirementMatchExpression::After(chrono::Utc.timestamp(1616890416, 0)),
            )]
        );
        assert!(data.is_empty());
    }

    #[test]
    fn parse_match_on_or_before() {
        let source =
            hex::decode("000000010000000a000000036b6579000000000c00000000605fca30").unwrap();

        let (els, data) = parse_code_requirements(&source).unwrap();

        assert_eq!(
            els,
            vec![CodeRequirementExpression::InfoPlistKeyField(
                "key".into(),
                CodeRequirementMatchExpression::OnOrBefore(chrono::Utc.timestamp(1616890416, 0)),
            )]
        );
        assert!(data.is_empty());
    }

    #[test]
    fn parse_match_on_or_after() {
        let source =
            hex::decode("000000010000000a000000036b6579000000000d00000000605fca30").unwrap();

        let (els, data) = parse_code_requirements(&source).unwrap();

        assert_eq!(
            els,
            vec![CodeRequirementExpression::InfoPlistKeyField(
                "key".into(),
                CodeRequirementMatchExpression::OnOrAfter(chrono::Utc.timestamp(1616890416, 0)),
            )]
        );
        assert!(data.is_empty());
    }
}