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

pub(crate) fn compact(input: &str) -> String {
    let plain = strip_ansi(input);
    let mut output = String::with_capacity(plain.len());
    let mut pending_space = false;
    for character in plain.chars() {
        if character.is_control() || character.is_whitespace() {
            pending_space = !output.is_empty();
            continue;
        }
        if pending_space {
            output.push(' ');
            pending_space = false;
        }
        output.push(character);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::{compact, strip_ansi};

    #[test]
    fn strip_ansi_removes_csi_sequences_but_keeps_prompt_text() {
        assert_eq!(strip_ansi("\u{1b}[32muser>\u{1b}[0m "), "user> ");
    }

    #[test]
    fn strip_ansi_removes_osc_sequences_terminated_by_bel() {
        assert_eq!(strip_ansi("\u{1b}]0;slide title\u{7}user>"), "user>");
    }

    #[test]
    fn strip_ansi_removes_osc_sequences_terminated_by_st() {
        assert_eq!(
            strip_ansi("\u{1b}]0;slide title\u{1b}\\\u{258c}"),
            "\u{258c}",
        );
    }

    #[test]
    fn compact_strips_terminal_sequences_and_collapses_whitespace() {
        assert_eq!(
            compact("  first\n\u{1b}[31msecond\u{1b}[0m\t third\u{7}  "),
            "first second third",
        );
    }
}
