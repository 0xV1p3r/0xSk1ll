//! Keeping credentials out of logs and error messages.

/// `text` with URL userinfo (`scheme://user:pass@`) and `Basic`/`Bearer`
/// credentials replaced by `***`, for logs and errors.
pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        let end = tail
            .find(|c: char| c == '/' || c.is_whitespace() || matches!(c, '\'' | '"' | '`' | '>' | ')'))
            .unwrap_or(tail.len());
        let authority = &tail[..end];
        match authority.rfind('@') {
            Some(at) => {
                out.push_str("***@");
                out.push_str(&authority[at + 1..]);
            }
            None => out.push_str(authority),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    for scheme in ["Basic ", "Bearer ", "basic ", "bearer "] {
        out = redact_after(&out, scheme);
    }
    out
}

fn redact_after(text: &str, marker: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(marker) {
        let (head, tail) = rest.split_at(i + marker.len());
        out.push_str(head);
        let end = tail.find(|c: char| c.is_whitespace() || matches!(c, '\'' | '"')).unwrap_or(tail.len());
        if end > 0 {
            out.push_str("***");
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction() {
        assert_eq!(
            redact("`git clone http://ws:tok@h:1/git/repo.git x` failed: 'https://a@b/c'"),
            "`git clone http://***@h:1/git/repo.git x` failed: 'https://***@b/c'"
        );
        assert_eq!(redact("http://h/git/repo.git"), "http://h/git/repo.git");
        assert_eq!(redact("Authorization: Basic d3M6dG9r here"), "Authorization: Basic *** here");
        assert_eq!(redact("Bearer abc"), "Bearer ***");
    }
}
