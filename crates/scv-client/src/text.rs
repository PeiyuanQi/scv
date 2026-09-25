//! Byte-bounded text, for limits that count bytes on the wire or on disk.

/// The longest prefix of `value` that fits in `max_bytes` without splitting
/// a character.
pub fn utf8_prefix(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

#[cfg(test)]
mod tests;
