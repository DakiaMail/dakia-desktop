//! Decoding for `text/plain; format=flowed` bodies (RFC 3676).

/// Decodes a `format=flowed` plain-text body.
///
/// Physical lines ending in a space are joined to the following line only when
/// both lines have the same quote depth. Space-stuffing is removed before
/// recognising quote prefixes. `delsp=yes` removes the flowed space while
/// joining; otherwise it remains as the separator. Signature delimiters are
/// always hard breaks, including inside quoted text.
pub(crate) fn decode_format_flowed(input: &str, delsp: bool) -> String {
    let lines = physical_lines(input)
        .into_iter()
        .map(parse_line)
        .collect::<Vec<_>>();
    let mut decoded = String::with_capacity(input.len());

    let mut index = 0;
    while let Some(first_line) = lines.get(index) {
        decoded.push_str(first_line.display);
        let mut current = first_line;

        while current.is_flowed
            && lines
                .get(index + 1)
                .is_some_and(|next| next.quote_depth == current.quote_depth)
        {
            if delsp {
                // `is_flowed` guarantees that this is the final ASCII space.
                decoded.pop();
            }

            index += 1;
            current = &lines[index];
            // Continuations keep their content but not their repeated quote
            // prefix (the first physical line supplied that prefix already).
            decoded.push_str(current.content);
        }

        decoded.push_str(current.line_ending);
        index += 1;
    }

    decoded
}

#[derive(Debug)]
struct PhysicalLine<'a> {
    text: &'a str,
    line_ending: &'a str,
}

fn physical_lines(input: &str) -> Vec<PhysicalLine<'_>> {
    let bytes = input.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0;
    let mut cursor = 0;

    while cursor < bytes.len() {
        let ending_len = match bytes[cursor] {
            b'\n' => 1,
            b'\r' if bytes.get(cursor + 1) == Some(&b'\n') => 2,
            b'\r' => 1,
            _ => {
                cursor += 1;
                continue;
            }
        };

        lines.push(PhysicalLine {
            text: &input[start..cursor],
            line_ending: &input[cursor..cursor + ending_len],
        });
        cursor += ending_len;
        start = cursor;
    }

    if start < input.len() {
        lines.push(PhysicalLine {
            text: &input[start..],
            line_ending: "",
        });
    }

    lines
}

#[derive(Debug)]
struct FlowedLine<'a> {
    /// The unstuffed physical line, retained for hard breaks and first lines.
    display: &'a str,
    /// The line without its quote prefix, used for flowed continuations.
    content: &'a str,
    quote_depth: usize,
    is_flowed: bool,
    line_ending: &'a str,
}

fn parse_line(line: PhysicalLine<'_>) -> FlowedLine<'_> {
    let display = line.text.strip_prefix(' ').unwrap_or(line.text);
    let mut content = display;
    let mut quote_depth = 0;

    while let Some(rest) = content.strip_prefix('>') {
        quote_depth += 1;
        content = rest;
    }
    if quote_depth > 0 {
        content = content.strip_prefix(' ').unwrap_or(content);
    }

    FlowedLine {
        display,
        content,
        quote_depth,
        // A signature delimiter has a trailing space, but RFC 3676 makes it a
        // fixed (non-flowed) line. This also applies to a quoted delimiter.
        is_flowed: display.ends_with(' ') && content != "-- ",
        line_ending: line.line_ending,
    }
}

#[cfg(test)]
mod tests {
    use super::decode_format_flowed;

    #[test]
    fn joins_soft_lines_and_preserves_the_separator_without_delsp() {
        assert_eq!(
            decode_format_flowed("A soft line \r\ncontinues.\r\nA hard break", false),
            "A soft line continues.\r\nA hard break"
        );
    }

    #[test]
    fn retains_hard_breaks() {
        assert_eq!(
            decode_format_flowed("First paragraph\nSecond paragraph", false),
            "First paragraph\nSecond paragraph"
        );
    }

    #[test]
    fn only_joins_lines_at_the_same_quote_depth() {
        assert_eq!(
            decode_format_flowed(
                "> First level \n>> Second level\n> Back at first \n> continued",
                false
            ),
            "> First level \n>> Second level\n> Back at first continued"
        );
    }

    #[test]
    fn removes_space_stuffing_before_processing_quotes() {
        assert_eq!(
            decode_format_flowed(
                " From the beginning\n > Quoted soft line \n > continuation",
                false
            ),
            "From the beginning\n> Quoted soft line continuation"
        );
    }

    #[test]
    fn delsp_removes_the_flowed_space() {
        assert_eq!(
            decode_format_flowed("A wrappedword \ncontinues", true),
            "A wrappedwordcontinues"
        );
    }

    #[test]
    fn signature_delimiter_is_not_a_soft_break() {
        assert_eq!(
            decode_format_flowed("-- \nSignature text", true),
            "-- \nSignature text"
        );
    }

    #[test]
    fn quoted_signature_delimiter_is_not_a_soft_break() {
        assert_eq!(
            decode_format_flowed("> -- \n> Signature text", true),
            "> -- \n> Signature text"
        );
    }
}
