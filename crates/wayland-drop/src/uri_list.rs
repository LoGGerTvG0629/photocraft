//! `text/uri-list` (RFC 2483) as file managers send it: one URI per line, CRLF or LF, `#`
//! comments. Only local `file:` URIs become paths.

use std::path::PathBuf;

/// The local paths in a `text/uri-list` body. `file:///p`, `file://localhost/p` and `file:/p` are
/// local; other hosts and schemes are skipped, as are paths that aren't absolute.
pub fn parse_uri_list(body: &str) -> Vec<PathBuf> {
    body.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).filter_map(file_path).collect()
}

fn file_path(uri: &str) -> Option<PathBuf> {
    let rest = strip_prefix_ignore_case(uri, "file:")?;
    let path = match rest.strip_prefix("//") {
        Some(authority_path) => {
            let slash = authority_path.find('/')?;
            let host = &authority_path[..slash];
            if !(host.is_empty() || host.eq_ignore_ascii_case("localhost")) {
                return None;
            }
            &authority_path[slash..]
        }
        None => rest,
    };
    // A query or fragment isn't part of the path.
    let path = path.split(['?', '#']).next().unwrap_or(path);
    let bytes = percent_decode(path)?;
    let path = bytes_to_path(bytes);
    path.is_absolute().then_some(path)
}

fn strip_prefix_ignore_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &s[prefix.len()..])
}

/// `%XX` escapes to bytes; `None` on a malformed escape. Paths are bytes on Unix, not UTF-8.
fn percent_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let hi = hex(bytes.next()?)?;
            let lo = hex(bytes.next()?)?;
            out.push(hi << 4 | lo);
        } else {
            out.push(b);
        }
    }
    Some(out)
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(unix)]
fn bytes_to_path(bytes: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(std::ffi::OsString::from_vec(bytes))
}

#[cfg(not(unix))]
fn bytes_to_path(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn nautilus_and_dolphin_lists() {
        let body = "file:///home/me/Pictures/wave.psd\r\nfile:///home/me/Pictures/sierra%20nevada.jpg\r\n";
        assert_eq!(parse_uri_list(body), vec![PathBuf::from("/home/me/Pictures/wave.psd"), PathBuf::from("/home/me/Pictures/sierra nevada.jpg")]);
    }

    #[test]
    fn comments_blank_lines_and_lf_only() {
        let body = "# from a file manager\n\nfile:///a.png\n  \n";
        assert_eq!(parse_uri_list(body), vec![PathBuf::from("/a.png")]);
    }

    #[test]
    fn localhost_and_short_forms_are_local() {
        assert_eq!(parse_uri_list("file://localhost/x/y.tif"), vec![PathBuf::from("/x/y.tif")]);
        assert_eq!(parse_uri_list("FILE://LocalHost/x.tif"), vec![PathBuf::from("/x.tif")]);
        assert_eq!(parse_uri_list("file:/x.tif"), vec![PathBuf::from("/x.tif")]);
    }

    #[test]
    fn remote_hosts_other_schemes_and_relative_paths_are_skipped() {
        let body = "file://nas/share/a.png\nhttps://example.com/b.png\nsmb://nas/c.png\nfile:relative.png\n";
        assert!(parse_uri_list(body).is_empty());
    }

    #[test]
    fn utf8_and_non_utf8_escapes() {
        use std::os::unix::ffi::OsStrExt;
        // 北斎.png in UTF-8.
        assert_eq!(parse_uri_list("file:///%E5%8C%97%E6%96%8E.png"), vec![PathBuf::from("/北斎.png")]);
        // A Latin-1 file name is still a valid Unix path.
        let p = &parse_uri_list("file:///caf%E9.png")[0];
        assert_eq!(p.as_os_str().as_bytes(), b"/caf\xe9.png");
    }

    #[test]
    fn malformed_escapes_query_and_fragment() {
        assert!(parse_uri_list("file:///bad%2.png\nfile:///bad%zz.png").is_empty());
        assert_eq!(parse_uri_list("file:///a.png?x=1#frag"), vec![PathBuf::from("/a.png")]);
        // An escaped '#' stays in the name.
        assert_eq!(parse_uri_list("file:///take%231.png"), vec![PathBuf::from("/take#1.png")]);
    }
}
