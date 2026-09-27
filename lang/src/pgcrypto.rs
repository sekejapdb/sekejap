//! pgcrypto-compatible functions, and PostgreSQL's `encode` / `decode`.
//!
//! | function | answers |
//! |---|---|
//! | `digest(data, type)` | `bytea`: md5, sha1, sha224, sha256, sha384, sha512 |
//! | `hmac(data, key, type)` | `bytea`, over the same hashes |
//! | `gen_random_bytes(n)` | `bytea` of `n` random bytes, 1..=1024 |
//! | `gen_salt(type [, rounds])` | `text`: `bf` (bcrypt, rounds 4..=31, default 6) or `md5` |
//! | `crypt(password, salt)` | `text`: bcrypt (`$2a$`, `$2b$`, `$2x$`, `$2y$`) or md5-crypt (`$1$`) |
//! | `encode(bytes, format)` | `text`: `hex` or `base64` (76-character lines, as PostgreSQL) |
//! | `decode(text, format)` | `bytea`: `hex` or `base64` |
//!
//! Every answer is checked against PostgreSQL 16 with pgcrypto
//! (`lang/tests/sql_pgcrypto.rs`), and every error carries PostgreSQL's
//! SQLSTATE. A `bytea` is written as PostgreSQL prints one, `\x` and the hex
//! digits, and a TEXT argument spelled that way is read as those bytes, which
//! is how a `digest` result reaches `encode`.
//!
//! **Deviation, stated:** PostgreSQL's `crypt` falls back to traditional DES
//! for a salt it does not recognise, and `gen_salt` offers `des` and `xdes`.
//! Those are 56-bit schemes from the 1970s; they are refused by name here,
//! never emulated. Use `gen_salt('bf')`.

use crate::compile::bytea_text;
use crate::{SqlError, SqlResult2, SqlValue};
use hmac::{Hmac, Mac};
use md5::Md5;
use sha1::Sha1;
use sha2::{Digest, Sha224, Sha256, Sha384, Sha512};

/// `22023 invalid_parameter_value`: an unknown algorithm, a rounds count out
/// of range, a malformed encoding.
const INVALID_PARAMETER_VALUE: &str = "22023";
/// `39000 external_routine_invocation_exception`: what pgcrypto raises for a
/// `gen_random_bytes` length out of range.
const EXTERNAL_ROUTINE: &str = "39000";
/// `0A000 feature_not_supported`.
const FEATURE_NOT_SUPPORTED: &str = "0A000";

/// The function names this module answers, upper-cased as the parser sees
/// them.
pub(crate) const FUNCTIONS: &[&str] = &[
    "DIGEST",
    "HMAC",
    "GEN_RANDOM_BYTES",
    "GEN_SALT",
    "CRYPT",
    "ENCODE",
    "DECODE",
];

pub(crate) fn is_function(name: &str) -> bool {
    FUNCTIONS.contains(&name.to_ascii_uppercase().as_str())
}

/// Call `name` on `args`. A NULL argument answers NULL, as every pgcrypto
/// function is STRICT.
pub(crate) fn call(name: &str, args: &[SqlValue]) -> SqlResult2<SqlValue> {
    if args.iter().any(|a| matches!(a, SqlValue::Null | SqlValue::Missing)) {
        return Ok(SqlValue::Null);
    }
    let upper = name.to_ascii_uppercase();
    let arity = |range: std::ops::RangeInclusive<usize>| -> SqlResult2<()> {
        if range.contains(&args.len()) {
            Ok(())
        } else {
            Err(SqlError::coded(
                "42883",
                format!("function {name} does not take {} argument(s)", args.len()),
            ))
        }
    };
    match upper.as_str() {
        "DIGEST" => {
            arity(2..=2)?;
            Ok(bytea(hash(&text_of(&args[1])?, &bytes_of(&args[0])?)?))
        }
        "HMAC" => {
            arity(3..=3)?;
            Ok(bytea(hmac_of(&text_of(&args[2])?, &bytes_of(&args[1])?, &bytes_of(&args[0])?)?))
        }
        "GEN_RANDOM_BYTES" => {
            arity(1..=1)?;
            let n = int_of(&args[0])?;
            if !(1..=1024).contains(&n) {
                return Err(SqlError::coded(EXTERNAL_ROUTINE, "Length not in range"));
            }
            Ok(bytea(random(n as usize)?))
        }
        "GEN_SALT" => {
            arity(1..=2)?;
            let rounds = match args.get(1) {
                Some(v) => Some(int_of(v)?),
                None => None,
            };
            gen_salt(&text_of(&args[0])?, rounds).map(SqlValue::Text)
        }
        "CRYPT" => {
            arity(2..=2)?;
            crypt(&text_of(&args[0])?, &text_of(&args[1])?).map(SqlValue::Text)
        }
        "ENCODE" => {
            arity(2..=2)?;
            encode(&bytes_of(&args[0])?, &text_of(&args[1])?).map(SqlValue::Text)
        }
        "DECODE" => {
            arity(2..=2)?;
            decode(&text_of(&args[0])?, &text_of(&args[1])?).map(bytea)
        }
        _ => Err(SqlError::coded("42883", format!("function {name} does not exist"))),
    }
}

fn bytea(bytes: Vec<u8>) -> SqlValue {
    SqlValue::Text(bytea_text(&bytes))
}

fn text_of(value: &SqlValue) -> SqlResult2<String> {
    match value {
        SqlValue::Text(t) => Ok(t.clone()),
        SqlValue::Int(i) => Ok(i.to_string()),
        other => Err(SqlError::coded(
            "42804",
            format!("a text argument is expected, not {other:?}"),
        )),
    }
}

fn int_of(value: &SqlValue) -> SqlResult2<i64> {
    match value {
        SqlValue::Int(i) => Ok(*i),
        SqlValue::Float(f) if f.fract() == 0.0 => Ok(*f as i64),
        other => Err(SqlError::coded(
            "42804",
            format!("an integer argument is expected, not {other:?}"),
        )),
    }
}

/// A bytea argument: `\x` and hex digits are those bytes, as PostgreSQL
/// reads a bytea literal; any other text is its UTF-8 bytes, as PostgreSQL
/// passes a text argument.
fn bytes_of(value: &SqlValue) -> SqlResult2<Vec<u8>> {
    let text = text_of(value)?;
    if let Some(hex) = text.strip_prefix("\\x") {
        if let Ok(bytes) = from_hex(hex) {
            return Ok(bytes);
        }
    }
    Ok(text.into_bytes())
}

fn from_hex(hex: &str) -> Result<Vec<u8>, char> {
    let digit = |c: char| c.to_digit(16).ok_or(c);
    let chars: Vec<char> = hex.chars().filter(|c| !c.is_whitespace()).collect();
    if chars.len() % 2 != 0 {
        return Err(*chars.last().unwrap_or(&' '));
    }
    chars
        .chunks(2)
        .map(|pair| Ok((digit(pair[0])? * 16 + digit(pair[1])?) as u8))
        .collect()
}

fn random(n: usize) -> SqlResult2<Vec<u8>> {
    let mut out = vec![0u8; n];
    sekejap_core::internal::random_bytes(&mut out).map_err(SqlError::from)?;
    Ok(out)
}

fn unknown_hash(kind: &str) -> SqlError {
    SqlError::coded(
        INVALID_PARAMETER_VALUE,
        format!("Cannot use \"{kind}\": No such hash algorithm"),
    )
}

fn hash(kind: &str, data: &[u8]) -> SqlResult2<Vec<u8>> {
    Ok(match kind.to_ascii_lowercase().as_str() {
        "md5" => Md5::digest(data).to_vec(),
        "sha1" => Sha1::digest(data).to_vec(),
        "sha224" => Sha224::digest(data).to_vec(),
        "sha256" => Sha256::digest(data).to_vec(),
        "sha384" => Sha384::digest(data).to_vec(),
        "sha512" => Sha512::digest(data).to_vec(),
        _ => return Err(unknown_hash(kind)),
    })
}

fn hmac_of(kind: &str, key: &[u8], data: &[u8]) -> SqlResult2<Vec<u8>> {
    macro_rules! mac {
        ($h:ty) => {{
            let mut mac = Hmac::<$h>::new_from_slice(key).expect("HMAC takes a key of any length");
            mac.update(data);
            mac.finalize().into_bytes().to_vec()
        }};
    }
    Ok(match kind.to_ascii_lowercase().as_str() {
        "md5" => mac!(Md5),
        "sha1" => mac!(Sha1),
        "sha224" => mac!(Sha224),
        "sha256" => mac!(Sha256),
        "sha384" => mac!(Sha384),
        "sha512" => mac!(Sha512),
        _ => return Err(unknown_hash(kind)),
    })
}

// ── encode / decode ───────────────────────────────────────────────────────

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn encode(bytes: &[u8], format: &str) -> SqlResult2<String> {
    match format.to_ascii_lowercase().as_str() {
        "hex" => Ok(bytes.iter().map(|b| format!("{b:02x}")).collect()),
        "base64" => {
            let mut out = String::new();
            let mut line = 0;
            for chunk in bytes.chunks(3) {
                // PostgreSQL breaks base64 output into 76-character lines.
                if line == 76 {
                    out.push('\n');
                    line = 0;
                }
                let n = (u32::from(chunk[0]) << 16)
                    | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
                    | u32::from(*chunk.get(2).unwrap_or(&0));
                for i in 0..4 {
                    if i <= chunk.len() {
                        out.push(BASE64[((n >> (18 - 6 * i)) & 63) as usize] as char);
                    } else {
                        out.push('=');
                    }
                }
                line += 4;
            }
            Ok(out)
        }
        "escape" => Err(SqlError::coded(
            FEATURE_NOT_SUPPORTED,
            "encode(..., 'escape') is not built; use 'hex' or 'base64'",
        )),
        other => Err(SqlError::coded(
            INVALID_PARAMETER_VALUE,
            format!("unrecognized encoding: \"{other}\""),
        )),
    }
}

fn decode(text: &str, format: &str) -> SqlResult2<Vec<u8>> {
    match format.to_ascii_lowercase().as_str() {
        "hex" => from_hex(text).map_err(|c| {
            SqlError::coded(
                INVALID_PARAMETER_VALUE,
                format!("invalid hexadecimal digit: \"{c}\""),
            )
        }),
        "base64" => {
            let mut bits = 0u32;
            let mut count = 0;
            let mut out = Vec::new();
            for c in text.chars().filter(|c| !c.is_whitespace()) {
                if c == '=' {
                    break;
                }
                let Some(v) = BASE64.iter().position(|b| *b as char == c) else {
                    return Err(SqlError::coded(
                        INVALID_PARAMETER_VALUE,
                        format!("invalid symbol \"{c}\" found while decoding base64 sequence"),
                    ));
                };
                bits = (bits << 6) | v as u32;
                count += 6;
                if count >= 8 {
                    count -= 8;
                    out.push((bits >> count) as u8);
                    bits &= (1 << count) - 1;
                }
            }
            Ok(out)
        }
        "escape" => Err(SqlError::coded(
            FEATURE_NOT_SUPPORTED,
            "decode(..., 'escape') is not built; use 'hex' or 'base64'",
        )),
        other => Err(SqlError::coded(
            INVALID_PARAMETER_VALUE,
            format!("unrecognized encoding: \"{other}\""),
        )),
    }
}

// ── crypt ─────────────────────────────────────────────────────────────────

/// The crypt(3) alphabet, and bcrypt's reordering of it.
const CRYPT64: &[u8; 64] = b"./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const BCRYPT64: &[u8; 64] = b"./ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

fn des_refused() -> SqlError {
    SqlError::coded(
        FEATURE_NOT_SUPPORTED,
        "crypt: the DES and extended-DES formats are not built -- they are 56-bit schemes -- so a salt must be bcrypt (`$2a$`, from gen_salt('bf')) or md5-crypt (`$1$`)",
    )
}

fn gen_salt(kind: &str, rounds: Option<i64>) -> SqlResult2<String> {
    match kind.to_ascii_lowercase().as_str() {
        "bf" => {
            let rounds = rounds.unwrap_or(6);
            if !(4..=31).contains(&rounds) {
                return Err(SqlError::coded(
                    INVALID_PARAMETER_VALUE,
                    "gen_salt: Incorrect number of rounds",
                ));
            }
            let salt = random(16)?;
            Ok(format!("$2a${rounds:02}${}", bcrypt64_encode(&salt)))
        }
        "md5" => {
            let bytes = random(8)?;
            let salt: String = bytes.iter().map(|b| CRYPT64[(*b & 63) as usize] as char).collect();
            Ok(format!("$1${salt}"))
        }
        "des" | "xdes" => Err(des_refused()),
        _ => Err(SqlError::coded(
            INVALID_PARAMETER_VALUE,
            "gen_salt: Unknown salt algorithm",
        )),
    }
}

fn crypt(password: &str, salt: &str) -> SqlResult2<String> {
    if let Some(version) = ["$2a$", "$2b$", "$2x$", "$2y$"].iter().find(|v| salt.starts_with(**v)) {
        return bcrypt_crypt(password, salt, version);
    }
    if let Some(rest) = salt.strip_prefix("$1$") {
        return Ok(md5_crypt(password.as_bytes(), rest));
    }
    Err(des_refused())
}

fn bad_salt() -> SqlError {
    SqlError::coded(INVALID_PARAMETER_VALUE, "invalid salt")
}

fn bcrypt_crypt(password: &str, salt: &str, version: &str) -> SqlResult2<String> {
    // `$2a$NN$` then 22 characters of salt; a whole hash may follow, which
    // is how `crypt(input, stored_hash)` verifies a password.
    let body = &salt[4..];
    let (cost, rest) = body.split_once('$').ok_or_else(bad_salt)?;
    let cost: u32 = cost.parse().map_err(|_| bad_salt())?;
    if !(4..=31).contains(&cost) || rest.len() < 22 {
        return Err(bad_salt());
    }
    let salt = bcrypt64_decode(&rest[..22]).ok_or_else(bad_salt)?;
    let mut raw = [0u8; 16];
    raw.copy_from_slice(&salt[..16]);
    // bcrypt reads at most 72 bytes of the password, as pgcrypto does.
    let password = &password.as_bytes()[..password.len().min(72)];
    let parts = bcrypt::hash_with_salt(password, cost, raw)
        .map_err(|e| SqlError::coded(INVALID_PARAMETER_VALUE, format!("crypt: {e}")))?;
    let version = match version {
        "$2b$" => bcrypt::Version::TwoB,
        "$2x$" => bcrypt::Version::TwoX,
        "$2y$" => bcrypt::Version::TwoY,
        _ => bcrypt::Version::TwoA,
    };
    Ok(parts.format_for_version(version))
}

/// bcrypt's base64: its own alphabet, no padding.
fn bcrypt64_encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..=chunk.len() {
            out.push(BCRYPT64[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

fn bcrypt64_decode(text: &str) -> Option<Vec<u8>> {
    let mut bits = 0u32;
    let mut count = 0;
    let mut out = Vec::new();
    for c in text.bytes() {
        let v = BCRYPT64.iter().position(|b| *b == c)? as u32;
        bits = (bits << 6) | v;
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
            bits &= (1 << count) - 1;
        }
    }
    Some(out)
}

/// md5-crypt, the `$1$` scheme of FreeBSD and pgcrypto: at most eight
/// characters of salt, one thousand rounds of MD5.
fn md5_crypt(password: &[u8], rest: &str) -> String {
    let salt = rest.split('$').next().unwrap_or("");
    let salt = &salt.as_bytes()[..salt.len().min(8)];
    let mut alt = Md5::new();
    alt.update(password);
    alt.update(salt);
    alt.update(password);
    let alt = alt.finalize();
    let mut ctx = Md5::new();
    ctx.update(password);
    ctx.update(b"$1$");
    ctx.update(salt);
    let mut left = password.len();
    while left > 0 {
        let take = left.min(16);
        ctx.update(&alt[..take]);
        left -= take;
    }
    let mut i = password.len();
    while i > 0 {
        if i & 1 == 1 {
            ctx.update([0u8]);
        } else {
            ctx.update(&password[..password.len().min(1)]);
        }
        i >>= 1;
    }
    let mut last = ctx.finalize();
    for round in 0..1000 {
        let mut next = Md5::new();
        if round & 1 == 1 {
            next.update(password);
        } else {
            next.update(last);
        }
        if round % 3 != 0 {
            next.update(salt);
        }
        if round % 7 != 0 {
            next.update(password);
        }
        if round & 1 == 1 {
            next.update(last);
        } else {
            next.update(password);
        }
        last = next.finalize();
    }
    let mut out = format!("$1${}$", String::from_utf8_lossy(salt));
    let mut to64 = |value: u32, n: usize| {
        let mut v = value;
        for _ in 0..n {
            out.push(CRYPT64[(v & 63) as usize] as char);
            v >>= 6;
        }
    };
    let b = |i: usize| u32::from(last[i]);
    to64((b(0) << 16) | (b(6) << 8) | b(12), 4);
    to64((b(1) << 16) | (b(7) << 8) | b(13), 4);
    to64((b(2) << 16) | (b(8) << 8) | b(14), 4);
    to64((b(3) << 16) | (b(9) << 8) | b(15), 4);
    to64((b(4) << 16) | (b(10) << 8) | b(5), 4);
    to64(b(11), 2);
    out
}
