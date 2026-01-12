use std::ops::Range;

use crate::syntax_kind::SyntaxKind;

#[derive(Debug, Clone, PartialEq)]
pub enum LiteralValue {
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    Char(char),
    String(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct LiteralError {
    pub message: String,
    /// Byte range within the provided literal text (not file offsets).
    pub span: Range<usize>,
}

fn err(message: impl Into<String>, span: Range<usize>) -> LiteralError {
    LiteralError {
        message: message.into(),
        span,
    }
}

pub fn parse_literal(kind: SyntaxKind, text: &str) -> Result<LiteralValue, LiteralError> {
    match kind {
        SyntaxKind::IntLiteral => Ok(LiteralValue::Int(parse_int_literal(text)?)),
        SyntaxKind::LongLiteral => Ok(LiteralValue::Long(parse_long_literal(text)?)),
        SyntaxKind::FloatLiteral => Ok(LiteralValue::Float(parse_float_literal(text)?)),
        SyntaxKind::DoubleLiteral => Ok(LiteralValue::Double(parse_double_literal(text)?)),
        SyntaxKind::CharLiteral => Ok(LiteralValue::Char(unescape_char_literal(text)?)),
        SyntaxKind::StringLiteral => Ok(LiteralValue::String(unescape_string_literal(text)?)),
        SyntaxKind::TextBlock => Ok(LiteralValue::String(unescape_text_block(text)?)),
        _ => Err(err(
            format!("Unsupported literal kind: {kind:?}"),
            0..text.len(),
        )),
    }
}

pub fn parse_int_literal(text: &str) -> Result<i32, LiteralError> {
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return Err(err("Empty int literal", 0..0));
    }

    let last_idx = bytes.len() - 1;
    if matches!(bytes[last_idx], b'l' | b'L') {
        return Err(err(
            "Int literal must not have `L` suffix",
            last_idx..last_idx + 1,
        ));
    }

    let end = bytes.len();
    let (base, prefix_len, is_decimal) = integer_base(bytes, end)?;
    let limit = if is_decimal {
        i32::MAX as u64
    } else {
        u32::MAX as u64
    };

    let value = parse_unsigned_integer(bytes, prefix_len, end, base, limit)?;
    if is_decimal {
        Ok(value as i32)
    } else {
        Ok(value as u32 as i32)
    }
}

pub fn parse_long_literal(text: &str) -> Result<i64, LiteralError> {
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return Err(err("Empty long literal", 0..0));
    }

    let suffix_pos = bytes.len().saturating_sub(1);
    let suffix = *bytes.get(suffix_pos).unwrap_or(&0);
    if !matches!(suffix, b'l' | b'L') {
        return Err(err(
            "Long literal is missing `L` suffix",
            suffix_pos..suffix_pos + 1,
        ));
    }

    if suffix_pos == 0 {
        return Err(err("Long literal is missing digits", 0..text.len()));
    }

    if bytes[suffix_pos - 1] == b'_' {
        return Err(err(
            "Underscore is not allowed immediately before long suffix",
            suffix_pos - 1..suffix_pos,
        ));
    }

    let end = suffix_pos;
    let (base, prefix_len, is_decimal) = integer_base(bytes, end)?;
    let limit = if is_decimal {
        i64::MAX as u64
    } else {
        u64::MAX
    };

    let value = parse_unsigned_integer(bytes, prefix_len, end, base, limit)?;
    Ok(value as i64)
}

fn integer_base(bytes: &[u8], end: usize) -> Result<(u32, usize, bool), LiteralError> {
    if end == 0 {
        return Err(err("Empty integer literal", 0..0));
    }

    if bytes[0] != b'0' {
        return Ok((10, 0, true));
    }

    if end >= 2 {
        match bytes[1] {
            b'x' | b'X' => return Ok((16, 2, false)),
            b'b' | b'B' => return Ok((2, 2, false)),
            _ => {}
        }
    }

    if end > 1 {
        // Octal-ish (leading 0 with more digits/underscores).
        return Ok((8, 1, false));
    }

    Ok((10, 0, true))
}

fn parse_unsigned_integer(
    bytes: &[u8],
    prefix_len: usize,
    end: usize,
    base: u32,
    limit: u64,
) -> Result<u64, LiteralError> {
    debug_assert!(prefix_len <= end);

    if end == 0 {
        return Err(err("Missing digits", 0..0));
    }

    if bytes[end - 1] == b'_' {
        return Err(err(
            "Trailing underscore is not allowed in numeric literal",
            end - 1..end,
        ));
    }

    if prefix_len == 2 {
        if end == 2 {
            return Err(err(
                "Missing digits after base prefix",
                prefix_len..prefix_len,
            ));
        }
        if bytes[prefix_len] == b'_' {
            return Err(err(
                "Underscore is not allowed immediately after base prefix",
                prefix_len..prefix_len + 1,
            ));
        }
    }

    let mut value: u64 = 0;
    let mut seen_digit = false;

    for (idx, &b) in bytes[..end].iter().enumerate().skip(prefix_len) {
        if b == b'_' {
            continue;
        }

        let digit = match base {
            2 => match b {
                b'0'..=b'1' => (b - b'0') as u64,
                _ => {
                    return Err(err(
                        format!("Invalid digit `{}` in binary literal", b as char),
                        idx..idx + 1,
                    ))
                }
            },
            8 => match b {
                b'0'..=b'7' => (b - b'0') as u64,
                _ => {
                    return Err(err(
                        format!("Invalid digit `{}` in octal literal", b as char),
                        idx..idx + 1,
                    ))
                }
            },
            10 => match b {
                b'0'..=b'9' => (b - b'0') as u64,
                _ => {
                    return Err(err(
                        format!("Invalid digit `{}` in decimal literal", b as char),
                        idx..idx + 1,
                    ))
                }
            },
            16 => match b {
                b'0'..=b'9' => (b - b'0') as u64,
                b'a'..=b'f' => (b - b'a' + 10) as u64,
                b'A'..=b'F' => (b - b'A' + 10) as u64,
                _ => {
                    return Err(err(
                        format!("Invalid digit `{}` in hexadecimal literal", b as char),
                        idx..idx + 1,
                    ))
                }
            },
            _ => unreachable!("unsupported base"),
        };

        seen_digit = true;
        value = value
            .checked_mul(base as u64)
            .and_then(|v| v.checked_add(digit))
            .ok_or_else(|| err("Integer literal is too large", 0..end))?;

        if value > limit {
            return Err(err("Integer literal is out of range", 0..end));
        }
    }

    if !seen_digit {
        return Err(err("Missing digits", prefix_len..end));
    }

    Ok(value)
}

pub fn parse_float_literal(text: &str) -> Result<f32, LiteralError> {
    parse_floating_literal(text, FloatTarget::F32)
}

pub fn parse_double_literal(text: &str) -> Result<f64, LiteralError> {
    parse_floating_literal(text, FloatTarget::F64)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FloatTarget {
    F32,
    F64,
}

fn parse_floating_literal<T>(text: &str, target: FloatTarget) -> Result<T, LiteralError>
where
    T: FromFloatTarget,
{
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return Err(err("Empty floating literal", 0..0));
    }

    let (main_end, had_suffix) = split_float_suffix(text, target)?;
    let main = &text[..main_end];

    if main.is_empty() {
        return Err(err("Missing digits", 0..text.len()));
    }

    if main.as_bytes().last() == Some(&b'_') {
        return Err(err(
            "Trailing underscore is not allowed in numeric literal",
            main_end - 1..main_end,
        ));
    }

    let value = if main.starts_with("0x") || main.starts_with("0X") {
        parse_hex_floating(main, target)?
    } else {
        parse_decimal_floating(main, had_suffix, target)?
    };

    Ok(T::from_target(value))
}

trait FromFloatTarget: Sized {
    fn from_target(value: FloatValue) -> Self;
}

enum FloatValue {
    F32(f32),
    F64(f64),
}

impl FromFloatTarget for f32 {
    fn from_target(value: FloatValue) -> Self {
        match value {
            FloatValue::F32(v) => v,
            FloatValue::F64(v) => v as f32,
        }
    }
}

impl FromFloatTarget for f64 {
    fn from_target(value: FloatValue) -> Self {
        match value {
            FloatValue::F32(v) => v as f64,
            FloatValue::F64(v) => v,
        }
    }
}

fn split_float_suffix(text: &str, target: FloatTarget) -> Result<(usize, bool), LiteralError> {
    let bytes = text.as_bytes();
    let last_idx = bytes.len() - 1;
    let last = bytes[last_idx];

    match target {
        FloatTarget::F32 => {
            if !matches!(last, b'f' | b'F') {
                return Err(err(
                    "Float literal must end with `f` suffix",
                    last_idx..last_idx + 1,
                ));
            }
            if last_idx > 0 && bytes[last_idx - 1] == b'_' {
                return Err(err(
                    "Underscore is not allowed immediately before float suffix",
                    last_idx - 1..last_idx,
                ));
            }
            Ok((last_idx, true))
        }
        FloatTarget::F64 => {
            if matches!(last, b'f' | b'F') {
                return Err(err(
                    "Double literal must not use `f` suffix",
                    last_idx..last_idx + 1,
                ));
            }
            if matches!(last, b'd' | b'D') {
                if last_idx > 0 && bytes[last_idx - 1] == b'_' {
                    return Err(err(
                        "Underscore is not allowed immediately before double suffix",
                        last_idx - 1..last_idx,
                    ));
                }
                return Ok((last_idx, true));
            }
            Ok((bytes.len(), false))
        }
    }
}

fn parse_decimal_floating(
    main: &str,
    had_suffix: bool,
    target: FloatTarget,
) -> Result<FloatValue, LiteralError> {
    validate_decimal_floating(main, had_suffix)?;
    let sanitized: String = main.chars().filter(|&ch| ch != '_').collect();
    match target {
        FloatTarget::F32 => sanitized
            .parse::<f32>()
            .map(FloatValue::F32)
            .map_err(|_| err("Invalid float literal", 0..main.len())),
        FloatTarget::F64 => sanitized
            .parse::<f64>()
            .map(FloatValue::F64)
            .map_err(|_| err("Invalid double literal", 0..main.len())),
    }
}

fn validate_decimal_floating(main: &str, had_suffix: bool) -> Result<(), LiteralError> {
    let bytes = main.as_bytes();
    if bytes.first() == Some(&b'_') {
        return Err(err(
            "Leading underscore is not allowed in numeric literal",
            0..1,
        ));
    }
    if bytes.last() == Some(&b'_') {
        return Err(err(
            "Trailing underscore is not allowed in numeric literal",
            bytes.len() - 1..bytes.len(),
        ));
    }

    let mut dot_idx: Option<usize> = None;
    let mut exp_idx: Option<usize> = None;

    for (idx, &b) in bytes.iter().enumerate() {
        match b {
            b'0'..=b'9' | b'_' => {}
            b'.' => {
                if exp_idx.is_some() {
                    return Err(err(
                        "Decimal point must appear before exponent",
                        idx..idx + 1,
                    ));
                }
                if dot_idx.replace(idx).is_some() {
                    return Err(err("Multiple decimal points in literal", idx..idx + 1));
                }
            }
            b'e' | b'E' => {
                if exp_idx.replace(idx).is_some() {
                    return Err(err("Multiple exponents in literal", idx..idx + 1));
                }
            }
            b'+' | b'-' => match exp_idx {
                Some(e) if idx == e + 1 => {}
                _ => {
                    return Err(err(
                        "Sign is only allowed immediately after exponent indicator",
                        idx..idx + 1,
                    ))
                }
            },
            _ => {
                return Err(err(
                    format!("Invalid character `{}` in floating literal", b as char),
                    idx..idx + 1,
                ))
            }
        }
    }

    if let Some(d) = dot_idx {
        if d > 0 && bytes[d - 1] == b'_' {
            return Err(err(
                "Underscore is not allowed adjacent to decimal point",
                d - 1..d,
            ));
        }
        if d + 1 < bytes.len() && bytes[d + 1] == b'_' {
            return Err(err(
                "Underscore is not allowed adjacent to decimal point",
                d + 1..d + 2,
            ));
        }
    }

    if let Some(e) = exp_idx {
        if e > 0 && bytes[e - 1] == b'_' {
            return Err(err(
                "Underscore is not allowed adjacent to exponent indicator",
                e - 1..e,
            ));
        }
        if e + 1 >= bytes.len() {
            return Err(err("Missing exponent digits", e..e + 1));
        }
        if bytes[e + 1] == b'_' {
            return Err(err(
                "Underscore is not allowed adjacent to exponent indicator",
                e + 1..e + 2,
            ));
        }
        if matches!(bytes[e + 1], b'+' | b'-') {
            if e + 2 >= bytes.len() {
                return Err(err("Missing exponent digits", e..e + 1));
            }
            if bytes[e + 2] == b'_' {
                return Err(err(
                    "Underscore is not allowed adjacent to exponent sign",
                    e + 2..e + 3,
                ));
            }
        }
    }

    let sig_end = exp_idx.unwrap_or(bytes.len());
    if let Some(d) = dot_idx {
        let left_has_digit = bytes[..d].iter().any(|b| b.is_ascii_digit());
        let right_has_digit = bytes[d + 1..sig_end].iter().any(|b| b.is_ascii_digit());
        if !left_has_digit && !right_has_digit {
            return Err(err("Missing digits in literal", 0..sig_end));
        }
    } else {
        let has_digit = bytes[..sig_end].iter().any(|b| b.is_ascii_digit());
        if !has_digit {
            return Err(err("Missing digits in literal", 0..sig_end));
        }
    }

    if let Some(e) = exp_idx {
        let mut exp_start = e + 1;
        if matches!(bytes.get(exp_start), Some(b'+' | b'-')) {
            exp_start += 1;
        }
        let exp_has_digit = bytes[exp_start..].iter().any(|b| b.is_ascii_digit());
        if !exp_has_digit {
            return Err(err("Missing exponent digits", e..e + 1));
        }
    }

    if !had_suffix && dot_idx.is_none() && exp_idx.is_none() {
        return Err(err(
            "Floating literal without suffix must contain a decimal point or exponent",
            0..main.len(),
        ));
    }

    Ok(())
}

fn parse_hex_floating(main: &str, target: FloatTarget) -> Result<FloatValue, LiteralError> {
    let bytes = main.as_bytes();
    if bytes.len() < 3 {
        return Err(err(
            "Incomplete hexadecimal floating literal",
            0..main.len(),
        ));
    }
    if bytes.get(2) == Some(&b'_') {
        return Err(err(
            "Underscore is not allowed immediately after `0x` prefix",
            2..3,
        ));
    }

    let p_idx = bytes
        .iter()
        .position(|b| matches!(b, b'p' | b'P'))
        .ok_or_else(|| {
            err(
                "Hexadecimal floating literal is missing binary exponent (`p`)",
                0..main.len(),
            )
        })?;

    if p_idx <= 2 {
        return Err(err(
            "Hexadecimal floating literal is missing significand digits",
            2..p_idx,
        ));
    }

    if p_idx == bytes.len() - 1 {
        return Err(err("Missing exponent digits", p_idx..p_idx + 1));
    }

    if bytes[p_idx - 1] == b'_' {
        return Err(err(
            "Underscore is not allowed adjacent to exponent indicator",
            p_idx - 1..p_idx,
        ));
    }
    if bytes[p_idx + 1] == b'_' {
        return Err(err(
            "Underscore is not allowed adjacent to exponent indicator",
            p_idx + 1..p_idx + 2,
        ));
    }
    if matches!(bytes[p_idx + 1], b'+' | b'-') {
        if p_idx + 2 >= bytes.len() {
            return Err(err("Missing exponent digits", p_idx..p_idx + 1));
        }
        if bytes[p_idx + 2] == b'_' {
            return Err(err(
                "Underscore is not allowed adjacent to exponent sign",
                p_idx + 2..p_idx + 3,
            ));
        }
    }
    if bytes.last() == Some(&b'_') {
        return Err(err(
            "Trailing underscore is not allowed in numeric literal",
            bytes.len() - 1..bytes.len(),
        ));
    }

    // Validate significand and build mantissa digits.
    let mut nibbles: Vec<u8> = Vec::new();
    let mut frac_digits: usize = 0;
    let mut seen_dot = false;
    let mut after_dot = false;
    let mut saw_digit = false;

    for idx in 2..p_idx {
        let b = bytes[idx];
        match b {
            b'_' => continue,
            b'.' => {
                if seen_dot {
                    return Err(err("Multiple decimal points in literal", idx..idx + 1));
                }
                if idx > 0 && bytes[idx - 1] == b'_' {
                    return Err(err(
                        "Underscore is not allowed adjacent to decimal point",
                        idx - 1..idx,
                    ));
                }
                if idx + 1 < p_idx && bytes[idx + 1] == b'_' {
                    return Err(err(
                        "Underscore is not allowed adjacent to decimal point",
                        idx + 1..idx + 2,
                    ));
                }
                seen_dot = true;
                after_dot = true;
            }
            _ => {
                let nibble = hex_value(b).ok_or_else(|| {
                    err(
                        format!("Invalid character `{}` in hexadecimal literal", b as char),
                        idx..idx + 1,
                    )
                })?;
                saw_digit = true;
                nibbles.push(nibble);
                if after_dot {
                    frac_digits += 1;
                }
            }
        }
    }

    if !saw_digit {
        return Err(err(
            "Hexadecimal floating literal is missing significand digits",
            2..p_idx,
        ));
    }

    // Validate exponent part and parse its value.
    let exp_span_start = p_idx + 1;
    let exp_part = &bytes[exp_span_start..];

    if exp_part.first() == Some(&b'_') || exp_part.last() == Some(&b'_') {
        return Err(err(
            "Underscore is not allowed at start/end of exponent",
            exp_span_start..bytes.len(),
        ));
    }

    for (i, &b) in exp_part.iter().enumerate() {
        if i == 0 && matches!(b, b'+' | b'-') {
            continue;
        }
        match b {
            b'0'..=b'9' | b'_' => {}
            _ => {
                return Err(err(
                    format!("Invalid character `{}` in exponent", b as char),
                    exp_span_start + i..exp_span_start + i + 1,
                ))
            }
        }
    }

    let exp_val = parse_signed_decimal_with_underscores(exp_part, exp_span_start)?;

    let frac_bits_adjust = (frac_digits as i64).saturating_mul(4);
    let exp2 = exp_val.saturating_sub(frac_bits_adjust);

    let trimmed = trim_leading_zero_nibbles(&nibbles);
    let Some(bit_len) = bit_len_nibbles(trimmed) else {
        return Ok(match target {
            FloatTarget::F32 => FloatValue::F32(0.0),
            FloatTarget::F64 => FloatValue::F64(0.0),
        });
    };

    let bits = match target {
        FloatTarget::F32 => binary_to_ieee_bits(trimmed, bit_len, exp2, FloatParams::F32),
        FloatTarget::F64 => binary_to_ieee_bits(trimmed, bit_len, exp2, FloatParams::F64),
    };

    Ok(match target {
        FloatTarget::F32 => FloatValue::F32(f32::from_bits(bits as u32)),
        FloatTarget::F64 => FloatValue::F64(f64::from_bits(bits)),
    })
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn parse_signed_decimal_with_underscores(
    bytes: &[u8],
    span_base: usize,
) -> Result<i64, LiteralError> {
    if bytes.is_empty() {
        return Err(err("Missing exponent digits", span_base..span_base));
    }

    let mut i = 0;
    let mut sign: i64 = 1;
    if bytes[0] == b'+' {
        i = 1;
    } else if bytes[0] == b'-' {
        sign = -1;
        i = 1;
    }

    let mut value: i64 = 0;
    let mut seen_digit = false;

    while i < bytes.len() {
        let b = bytes[i];
        i += 1;
        if b == b'_' {
            continue;
        }
        let digit = match b {
            b'0'..=b'9' => (b - b'0') as i64,
            _ => {
                return Err(err(
                    format!("Invalid digit `{}` in exponent", b as char),
                    span_base + i - 1..span_base + i,
                ))
            }
        };

        seen_digit = true;
        value = value.saturating_mul(10).saturating_add(digit);
    }

    if !seen_digit {
        return Err(err(
            "Missing exponent digits",
            span_base..span_base + bytes.len(),
        ));
    }

    Ok(value.saturating_mul(sign))
}

#[derive(Debug, Clone, Copy)]
enum FloatParams {
    F32,
    F64,
}

fn binary_to_ieee_bits(nibbles: &[u8], bit_len: usize, exp2: i64, params: FloatParams) -> u64 {
    let (frac_bits, exp_bias, min_exp, max_exp) = match params {
        FloatParams::F32 => (23usize, 127i64, -126i64, 127i64),
        FloatParams::F64 => (52usize, 1023i64, -1022i64, 1023i64),
    };
    let precision = frac_bits + 1;
    let bit_len_i64 = bit_len as i64;
    let mut e = exp2.saturating_add(bit_len_i64).saturating_sub(1);

    // Infinity.
    if e > max_exp {
        return (((max_exp + exp_bias + 1) as u64) << frac_bits) & !((1u64 << frac_bits) - 1);
    }

    if e >= min_exp {
        // Normal.
        let mut q: u64;

        if bit_len > precision {
            let shift = bit_len - precision;
            q = extract_msb_bits(nibbles, bit_len, precision);
            let guard = get_bit(nibbles, shift - 1);
            let sticky = low_bits_nonzero(nibbles, shift - 1);

            if guard == 1 && (sticky || (q & 1 == 1)) {
                q += 1;
                if q == (1u64 << precision) {
                    q >>= 1;
                    e = e.saturating_add(1);
                    if e > max_exp {
                        return (((max_exp + exp_bias + 1) as u64) << frac_bits)
                            & !((1u64 << frac_bits) - 1);
                    }
                }
            }
        } else {
            let mut m_val: u64 = 0;
            for &n in nibbles {
                m_val = (m_val << 4) | n as u64;
            }
            q = m_val << (precision - bit_len);
        }

        let exp_field = (e + exp_bias) as u64;
        let frac_mask = (1u64 << frac_bits) - 1;
        let frac = q & frac_mask;
        return (exp_field << frac_bits) | frac;
    }

    // Subnormal.
    // fraction = round(m * 2^(exp2 + frac_bits - min_exp))
    let scale = (frac_bits as i64).saturating_sub(min_exp);
    let k = exp2.saturating_add(scale);

    let fraction: u64 = if k >= 0 {
        let shift_left = k as usize;
        let mut m_val: u64 = 0;
        for &n in nibbles {
            m_val = (m_val << 4) | n as u64;
        }
        if shift_left >= 64 {
            u64::MAX
        } else {
            m_val << shift_left
        }
    } else {
        let shift_right = (-k) as usize;
        let q_bit_len = bit_len.saturating_sub(shift_right);
        let q_bit_len = q_bit_len.min(frac_bits + 1);

        let mut q: u64 = 0;
        if q_bit_len > 0 {
            for i in (0..q_bit_len).rev() {
                let bit = get_bit(nibbles, shift_right + i);
                q = (q << 1) | bit as u64;
            }
        }

        let guard = if shift_right == 0 {
            0
        } else {
            get_bit(nibbles, shift_right - 1)
        };
        let sticky = if shift_right <= 1 {
            false
        } else {
            low_bits_nonzero(nibbles, shift_right - 1)
        };

        if guard == 1 && (sticky || (q & 1 == 1)) {
            q = q.saturating_add(1);
        }
        q
    };

    if fraction == 0 {
        return 0;
    }

    let max_sub = 1u64 << frac_bits;
    if fraction >= max_sub {
        // Rounded up into the smallest normal number.
        let exp_field = 1u64;
        return exp_field << frac_bits;
    }

    fraction
}

fn trim_leading_zero_nibbles(nibbles: &[u8]) -> &[u8] {
    let mut i = 0;
    while i < nibbles.len() && nibbles[i] == 0 {
        i += 1;
    }
    &nibbles[i..]
}

fn bit_len_nibbles(nibbles: &[u8]) -> Option<usize> {
    if nibbles.is_empty() {
        return None;
    }
    let mut idx = 0;
    while idx < nibbles.len() && nibbles[idx] == 0 {
        idx += 1;
    }
    if idx == nibbles.len() {
        return None;
    }
    let first = nibbles[idx];
    let leading_zeros_in_byte = first.leading_zeros() as usize;
    // `first` is a 4-bit nibble.
    let leading_zeros_in_nibble = leading_zeros_in_byte.saturating_sub(4);
    let total_bits = (nibbles.len() - idx) * 4;
    Some(total_bits - leading_zeros_in_nibble)
}

fn get_bit(nibbles: &[u8], bit_pos: usize) -> u8 {
    let total_bits = nibbles.len() * 4;
    if bit_pos >= total_bits || nibbles.is_empty() {
        return 0;
    }
    let nibble_from_lsb = bit_pos / 4;
    let bit_in_nibble = bit_pos % 4;
    let idx = nibbles.len() - 1 - nibble_from_lsb;
    (nibbles[idx] >> bit_in_nibble) & 1
}

fn extract_msb_bits(nibbles: &[u8], bit_len: usize, count: usize) -> u64 {
    let take = count.min(bit_len);
    let mut out: u64 = 0;
    for i in 0..take {
        let bit = get_bit(nibbles, bit_len - 1 - i);
        out = (out << 1) | bit as u64;
    }
    out
}

fn low_bits_nonzero(nibbles: &[u8], bits_count: usize) -> bool {
    if bits_count == 0 {
        return false;
    }
    if nibbles.is_empty() {
        return false;
    }
    let total_bits = nibbles.len() * 4;
    let bits_count = bits_count.min(total_bits);

    let full_nibbles = bits_count / 4;
    let rem_bits = bits_count % 4;

    for i in 0..full_nibbles {
        let idx = nibbles.len() - 1 - i;
        if nibbles[idx] != 0 {
            return true;
        }
    }

    if rem_bits > 0 {
        let idx = nibbles.len().saturating_sub(1 + full_nibbles);
        if idx < nibbles.len() {
            let mask = (1u8 << rem_bits) - 1;
            if (nibbles[idx] & mask) != 0 {
                return true;
            }
        }
    }

    false
}

pub fn unescape_char_literal(text: &str) -> Result<char, LiteralError> {
    let bytes = text.as_bytes();
    if bytes.len() < 2 || bytes.first() != Some(&b'\'') || bytes.last() != Some(&b'\'') {
        return Err(err("Invalid char literal", 0..text.len()));
    }

    let mut out = String::new();
    unescape_java_string_like(text, 1, text.len() - 1, false, &mut out)?;
    let mut utf16 = out.encode_utf16();
    let ch = out
        .chars()
        .next()
        .ok_or_else(|| err("Empty char literal", 0..text.len()))?;
    let Some(_) = utf16.next() else {
        return Err(err("Empty char literal", 0..text.len()));
    };
    if utf16.next().is_some() {
        return Err(err(
            "Char literal must contain exactly one character",
            0..text.len(),
        ));
    }
    Ok(ch)
}

pub fn unescape_string_literal(text: &str) -> Result<String, LiteralError> {
    let bytes = text.as_bytes();
    if bytes.len() < 2 || bytes.first() != Some(&b'"') || bytes.last() != Some(&b'"') {
        return Err(err("Invalid string literal", 0..text.len()));
    }
    let mut out = String::new();
    unescape_java_string_like(text, 1, text.len() - 1, false, &mut out)?;
    Ok(out)
}

pub fn unescape_text_block(text: &str) -> Result<String, LiteralError> {
    if !text.starts_with("\"\"\"") || !text.ends_with("\"\"\"") || text.len() < 6 {
        return Err(err("Invalid text block literal", 0..text.len()));
    }

    let bytes = text.as_bytes();
    let closing_start = text.len() - 3;

    // Compute indentation width `N` from the closing delimiter line.
    let mut closing_line_start = 0usize;
    let mut i = closing_start;
    while i > 0 {
        i -= 1;
        match bytes[i] {
            b'\n' | b'\r' => {
                closing_line_start = i + 1;
                break;
            }
            _ => {}
        }
    }

    let mut n = 0usize;
    while closing_line_start + n < closing_start {
        match bytes[closing_line_start + n] {
            b' ' | b'\t' => n += 1,
            _ => break,
        }
    }

    // After opening delimiter, require optional whitespace and a line terminator.
    let mut content_start = 3usize;
    while content_start < closing_start {
        match bytes[content_start] {
            b' ' | b'\t' => content_start += 1,
            _ => break,
        }
    }

    if content_start >= closing_start {
        return Err(err(
            "Missing line terminator after opening delimiter",
            3..closing_start,
        ));
    }

    match bytes[content_start] {
        b'\n' => content_start += 1,
        b'\r' => {
            content_start += 1;
            if content_start < closing_start && bytes[content_start] == b'\n' {
                content_start += 1;
            }
        }
        _ => {
            return Err(err(
                "Missing line terminator after opening delimiter",
                content_start..content_start + 1,
            ))
        }
    }

    let mut out = String::new();
    let mut idx = content_start;
    let mut at_line_start = true;

    while idx < closing_start {
        if at_line_start {
            let mut removed = 0usize;
            while removed < n && idx < closing_start {
                match bytes[idx] {
                    b' ' | b'\t' => {
                        idx += 1;
                        removed += 1;
                    }
                    _ => break,
                }
            }
            at_line_start = false;
            continue;
        }

        let b = bytes[idx];
        match b {
            b'\\' => {
                // Line continuation: backslash + line terminator removes the terminator.
                if idx + 1 < closing_start && matches!(bytes[idx + 1], b'\n' | b'\r') {
                    idx += 2;
                    if bytes[idx - 1] == b'\r' && idx < closing_start && bytes[idx] == b'\n' {
                        idx += 1;
                    }
                    at_line_start = true;
                    continue;
                }

                idx = unescape_java_escape(text, idx, closing_start, true, &mut out)?;
            }
            b'\n' => {
                out.push('\n');
                idx += 1;
                at_line_start = true;
            }
            b'\r' => {
                out.push('\n');
                idx += 1;
                if idx < closing_start && bytes[idx] == b'\n' {
                    idx += 1;
                }
                at_line_start = true;
            }
            _ => {
                if b < 0x80 {
                    out.push(b as char);
                    idx += 1;
                } else {
                    let ch = text[idx..closing_start]
                        .chars()
                        .next()
                        .unwrap_or('\u{FFFD}');
                    out.push(ch);
                    idx += ch.len_utf8();
                }
            }
        }
    }

    Ok(out)
}

fn unescape_java_string_like(
    text: &str,
    start: usize,
    end: usize,
    allow_line_continuation: bool,
    out: &mut String,
) -> Result<(), LiteralError> {
    let bytes = text.as_bytes();
    let mut idx = start;

    while idx < end {
        let b = bytes[idx];
        match b {
            b'\\' => {
                idx = unescape_java_escape(text, idx, end, allow_line_continuation, out)?;
            }
            b'\n' | b'\r' => {
                return Err(err(
                    "Line terminator is not allowed in string/char literal",
                    idx..idx + 1,
                ))
            }
            _ => {
                if b < 0x80 {
                    out.push(b as char);
                    idx += 1;
                } else {
                    let ch = text[idx..end].chars().next().unwrap_or('\u{FFFD}');
                    out.push(ch);
                    idx += ch.len_utf8();
                }
            }
        }
    }

    Ok(())
}

fn unescape_java_escape(
    text: &str,
    idx: usize,
    end: usize,
    allow_line_continuation: bool,
    out: &mut String,
) -> Result<usize, LiteralError> {
    let bytes = text.as_bytes();
    debug_assert_eq!(bytes[idx], b'\\');
    if idx + 1 >= end {
        return Err(err("Unterminated escape sequence", idx..end));
    }

    let next = bytes[idx + 1];
    if allow_line_continuation && matches!(next, b'\n' | b'\r') {
        // Backslash + line terminator is a line continuation.
        let mut new_idx = idx + 2;
        if next == b'\r' && new_idx < end && bytes[new_idx] == b'\n' {
            new_idx += 1;
        }
        return Ok(new_idx);
    }

    match next {
        b'b' => {
            out.push('\u{0008}');
            Ok(idx + 2)
        }
        b't' => {
            out.push('\t');
            Ok(idx + 2)
        }
        b'n' => {
            out.push('\n');
            Ok(idx + 2)
        }
        b'f' => {
            out.push('\u{000C}');
            Ok(idx + 2)
        }
        b'r' => {
            out.push('\r');
            Ok(idx + 2)
        }
        b'"' => {
            out.push('"');
            Ok(idx + 2)
        }
        b'\'' => {
            out.push('\'');
            Ok(idx + 2)
        }
        b'\\' => {
            out.push('\\');
            Ok(idx + 2)
        }
        b's' => {
            out.push(' ');
            Ok(idx + 2)
        }
        b'u' => {
            let mut j = idx + 2;
            while j < end && bytes[j] == b'u' {
                j += 1;
            }
            if j + 4 > end {
                return Err(err("Incomplete unicode escape", idx..end));
            }
            let mut value: u32 = 0;
            for k in 0..4 {
                let pos = j + k;
                let b = bytes[pos];
                let digit = hex_value(b).ok_or_else(|| {
                    err(
                        format!("Invalid hex digit `{}` in unicode escape", b as char),
                        pos..pos + 1,
                    )
                })?;
                value = (value << 4) | digit as u32;
            }

            let ch = char::from_u32(value)
                .ok_or_else(|| err("Unicode escape is not a valid scalar value", idx..j + 4))?;
            out.push(ch);
            Ok(j + 4)
        }
        b'0'..=b'7' => {
            let first = next;
            let max_digits = if first <= b'3' { 3 } else { 2 };
            let mut j = idx + 1;
            let mut value: u32 = 0;
            let mut count = 0;
            while count < max_digits && j < end {
                let b = bytes[j];
                if matches!(b, b'0'..=b'7') {
                    value = value * 8 + (b - b'0') as u32;
                    j += 1;
                    count += 1;
                } else {
                    break;
                }
            }
            let ch = char::from_u32(value).ok_or_else(|| {
                err(
                    "Octal escape is not a valid scalar value",
                    idx..idx + 1 + count,
                )
            })?;
            out.push(ch);
            Ok(j)
        }
        _ => Err(err(
            format!("Unknown escape sequence `\\{}`", next as char),
            idx..idx + 2,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_literals_decimal_and_hex_twos_complement() {
        assert_eq!(parse_int_literal("2147483647").unwrap(), 2147483647);
        assert!(parse_int_literal("2147483648").is_err());
        assert_eq!(parse_int_literal("0xFFFF_FFFF").unwrap(), -1);
        assert_eq!(parse_int_literal("0x8000_0000").unwrap(), i32::MIN);
    }

    #[test]
    fn long_literals_suffix_and_twos_complement() {
        assert_eq!(
            parse_long_literal("9223372036854775807L").unwrap(),
            i64::MAX
        );
        assert!(parse_long_literal("9223372036854775808L").is_err());
        assert_eq!(parse_long_literal("0xFFFF_FFFF_FFFF_FFFFL").unwrap(), -1);
    }

    #[test]
    fn float_and_double_decimal_and_hex() {
        assert_eq!(parse_float_literal("1f").unwrap(), 1.0f32);
        assert_eq!(parse_double_literal("1.").unwrap(), 1.0f64);
        assert_eq!(parse_double_literal("0x1p1").unwrap(), 2.0f64);
    }

    #[test]
    fn string_and_char_escapes() {
        assert_eq!(unescape_char_literal("'\\n'").unwrap(), '\n');
        assert_eq!(unescape_string_literal("\"a\\tb\"").unwrap(), "a\tb");
        assert_eq!(unescape_string_literal("\"\\s\"").unwrap(), " ");
        assert_eq!(unescape_string_literal("\"\\141\"").unwrap(), "a");
        assert_eq!(unescape_string_literal("\"\\u0041\"").unwrap(), "A");
    }

    #[test]
    fn text_block_requires_line_terminator() {
        assert!(unescape_text_block("\"\"\"hi\"\"\"").is_err());
    }
}
