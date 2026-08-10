/// Strip the ANSI control sequences emitted by terminal applications while
/// preserving their printable text. This deliberately handles the CSI and
/// OSC families used by supported agent TUIs; unknown two-byte escapes are
/// discarded rather than exposed in classifier/search text.
pub(crate) fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b && i + 1 < bytes.len() {
            let next = bytes[i + 1];
            if next == b'[' {
                i += 2;
                while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                    i += 1;
                }
                i = (i + 1).min(bytes.len());
                continue;
            }
            if next == b']' {
                i += 2;
                while i < bytes.len() && bytes[i] != 0x07 {
                    if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == 0x07 {
                    i += 1;
                }
                continue;
            }
            i += 2;
            continue;
        }
        if let Some(character) = input[i..].chars().next() {
            out.push(character);
            i += character.len_utf8();
        } else {
            break;
        }
    }
    out
}
