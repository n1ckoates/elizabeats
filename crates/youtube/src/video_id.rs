//! Recognizing YouTube links typed into the search box.

use url::Url;

/// Extracts the video ID from a YouTube or YouTube Music link.
///
/// Supports `youtu.be/<id>`, `*.youtube.com/watch?v=<id>` and
/// `*.youtube.com/{shorts,embed,live}/<id>`. Returns `None` for anything
/// else (including plain search text) or when the ID isn't 11 characters of
/// `A–Z a–z 0–9 _ -`.
pub fn parse_video_id(input: &str) -> Option<String> {
    let url = Url::parse(input.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let mut segments = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|segment| !segment.is_empty());

    let candidate = if host == "youtu.be" {
        segments.next().map(str::to_owned)
    } else if host == "youtube.com" || host.ends_with(".youtube.com") {
        let query_id = url
            .query_pairs()
            .find(|(key, value)| key == "v" && !value.is_empty())
            .map(|(_, value)| value.into_owned());
        query_id.or_else(|| match segments.next() {
            Some("shorts" | "embed" | "live") => segments.next().map(str::to_owned),
            _ => None,
        })
    } else {
        None
    };
    candidate.filter(|id| is_valid_video_id(id))
}

/// True for an 11-character YouTube video ID.
pub(crate) fn is_valid_video_id(id: &str) -> bool {
    id.len() == 11
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(input: &str) -> Option<String> {
        parse_video_id(input)
    }

    #[test]
    fn recognizes_supported_links() {
        let id = Some("FGBhQbmPwH8".to_owned());
        assert_eq!(parse("https://youtu.be/FGBhQbmPwH8"), id);
        assert_eq!(parse("https://youtu.be/FGBhQbmPwH8?si=abc&t=10"), id);
        assert_eq!(parse("https://www.youtube.com/watch?v=FGBhQbmPwH8"), id);
        assert_eq!(
            parse("https://youtube.com/watch?feature=x&v=FGBhQbmPwH8"),
            id
        );
        assert_eq!(
            parse("https://music.youtube.com/watch?v=FGBhQbmPwH8&list=RDAMVM"),
            id
        );
        assert_eq!(parse("https://m.youtube.com/watch?v=FGBhQbmPwH8"), id);
        assert_eq!(parse("https://www.youtube.com/shorts/FGBhQbmPwH8"), id);
        assert_eq!(
            parse("https://www.youtube.com/embed/FGBhQbmPwH8?start=3"),
            id
        );
        assert_eq!(parse("https://www.youtube.com/live/FGBhQbmPwH8"), id);
        assert_eq!(parse("  http://WWW.YouTube.com/watch?v=FGBhQbmPwH8  "), id);
    }

    #[test]
    fn rejects_everything_else() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("daft punk one more time"), None);
        assert_eq!(parse("youtube.com/watch?v=FGBhQbmPwH8"), None); // no scheme
        assert_eq!(parse("ftp://youtu.be/FGBhQbmPwH8"), None);
        assert_eq!(parse("https://notyoutube.com/watch?v=FGBhQbmPwH8"), None);
        assert_eq!(
            parse("https://youtube.com.evil.com/watch?v=FGBhQbmPwH8"),
            None
        );
        assert_eq!(parse("https://www.youtube.com/watch?v=short"), None);
        assert_eq!(parse("https://www.youtube.com/watch?v=FGBhQbmPwH8x"), None);
        assert_eq!(parse("https://www.youtube.com/watch?v=../../etc/pa"), None);
        assert_eq!(parse("https://www.youtube.com/channel/UC1234567890"), None);
        assert_eq!(parse("https://www.youtube.com/playlist?list=PL123"), None);
        assert_eq!(parse("https://youtu.be/"), None);
    }

    #[test]
    fn validates_ids() {
        assert!(is_valid_video_id("a-b_C123456"));
        assert!(!is_valid_video_id("a-b_C12345"));
        assert!(!is_valid_video_id("a-b_C12345/"));
        assert!(!is_valid_video_id("ä-b_C12345"));
    }
}
