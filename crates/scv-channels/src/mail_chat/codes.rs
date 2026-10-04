//! Approval codes and message handles: short, random, and easy to type.
//!
//! A code (six characters) names one action for one answer: the owner
//! approves an action by sending `approve CODE` in the mail chat. A handle
//! (`#` and four characters) names a reported message in the owner's
//! commands. Both use an alphabet without the letters and digits people
//! confuse (no `I`, `L`, `O`, `U`, `0`, or `1`), are drawn from the
//! operating system's random source, and are matched case-insensitively.
//! A code is not a secret, since only the owner can answer with it; it
//! binds an answer to one action, and a new one is drawn whenever an action
//! is offered again.

/// The characters codes and handles are made of.
pub(crate) const ALPHABET: &[u8; 30] = b"ABCDEFGHJKMNPQRSTVWXYZ23456789";
/// Characters in a code.
pub(crate) const CODE_LEN: usize = 6;
/// Characters in a handle, after its `#`.
pub(crate) const HANDLE_LEN: usize = 4;

/// `len` characters of [`ALPHABET`], each uniformly random.
#[cfg(feature = "email")]
fn random(len: usize) -> String {
    let mut out = String::with_capacity(len);
    while out.len() < len {
        // Version 4 UUIDs come from the operating system's random source;
        // bytes 6 and 8 hold its fixed version and variant bits, so only
        // the others are used.
        let bytes = uuid::Uuid::new_v4().into_bytes();
        for (index, byte) in bytes.into_iter().enumerate() {
            // 240 = 8 × 30: rejecting the rest keeps every character equally
            // likely.
            if index != 6 && index != 8 && byte < 240 && out.len() < len {
                out.push(char::from(ALPHABET[usize::from(byte % 30)]));
            }
        }
    }
    out
}

/// A new approval code.
#[cfg(feature = "email")]
pub(crate) fn new_code() -> String {
    random(CODE_LEN)
}

/// A new handle, without its `#`.
#[cfg(feature = "email")]
pub(crate) fn new_handle() -> String {
    random(HANDLE_LEN)
}

/// `text` as a code, uppercased, when it is one.
pub(crate) fn code(text: &str) -> Option<String> {
    word(text, CODE_LEN)
}

/// `text` as a handle, without its `#` (ASCII or full-width) and uppercased,
/// when it is one.
pub(crate) fn handle(text: &str) -> Option<String> {
    let text = text
        .strip_prefix('#')
        .or_else(|| text.strip_prefix('＃'))
        .unwrap_or(text);
    word(text, HANDLE_LEN)
}

fn word(text: &str, len: usize) -> Option<String> {
    let upper = text.to_ascii_uppercase();
    (upper.len() == len && upper.bytes().all(|byte| ALPHABET.contains(&byte))).then_some(upper)
}

#[cfg(test)]
mod tests;
