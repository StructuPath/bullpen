//! Minimal server-sent-events parsing over a fully buffered body.
//!
//! Bullpen presents blocking turn results, so streams are buffered and the
//! `data:` payloads extracted after the fact. Incremental delta events become
//! interesting at M1 (streaming); this parser's shape allows that upgrade
//! without changing callers' event handling.

/// Yield the JSON payload of every `data:` line, skipping blanks and `[DONE]`.
pub(crate) fn data_lines(body: &str) -> impl Iterator<Item = &str> {
    body.lines().filter_map(|line| {
        let line = line.trim();
        let data = line.strip_prefix("data:")?.trim();
        if data.is_empty() || data == "[DONE]" {
            None
        } else {
            Some(data)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_data_skips_noise() {
        let body = "event: ping\n\ndata: {\"a\":1}\n\ndata:\n\ndata: [DONE]\n\ndata:{\"b\":2}\n";
        let got: Vec<&str> = data_lines(body).collect();
        assert_eq!(got, vec!["{\"a\":1}", "{\"b\":2}"]);
    }
}
