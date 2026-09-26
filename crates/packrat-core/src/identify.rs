//! Turning a disc's volume label into a guess at what it contains.
//!
//! DVD volume labels are short, uppercased and separator-starved
//! (`DRAGON_BALL_S1_D1`), so this is deliberately forgiving: extract whatever
//! year/season/disc hints exist and strip the rest down to a plausible title.

/// What a volume label suggests.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LabelInfo {
    /// Human title, title-cased.
    pub title: String,
    /// Four-digit release year, when present.
    pub year: Option<u16>,
    /// Season number, when present.
    pub season: Option<u16>,
    /// Disc number within the season, when present.
    pub disc: Option<u16>,
    /// `true` when the label looks like a TV/season disc.
    pub looks_like_tv: bool,
}

/// Parse a volume label such as `DRAGON_BALL_S1_D1`.
pub fn parse_label(label: &str) -> LabelInfo {
    let normalized: String = label
        .chars()
        .map(|c| if c == '_' || c == '.' { ' ' } else { c })
        .collect();
    let words: Vec<&str> = normalized.split_whitespace().collect();
    let upper: Vec<String> = words.iter().map(|w| w.to_ascii_uppercase()).collect();

    let year = upper.iter().find_map(|w| parse_year(w));
    let season_hit = find_season(&upper);
    let disc_hit = find_disc(&upper);

    let mut consumed: Vec<usize> = Vec::new();
    if let Some((_, indices)) = &season_hit {
        consumed.extend(indices);
    }
    if let Some((_, indices)) = &disc_hit {
        consumed.extend(indices);
    }

    let title_words: Vec<&str> = words
        .iter()
        .enumerate()
        .filter(|(i, w)| !consumed.contains(i) && !is_noise(&w.to_ascii_uppercase()))
        .map(|(_, w)| *w)
        .collect();

    LabelInfo {
        title: title_case(&title_words.join(" ")),
        year,
        season: season_hit.as_ref().map(|(n, _)| *n),
        disc: disc_hit.as_ref().map(|(n, _)| *n),
        looks_like_tv: season_hit.is_some()
            || upper.iter().any(|w| {
                matches!(
                    w.as_str(),
                    "SEASON" | "SERIES" | "DISC" | "DISK" | "COMPLETE"
                )
            }),
    }
}

fn parse_year(word: &str) -> Option<u16> {
    if word.len() == 4
        && (word.starts_with("19") || word.starts_with("20"))
        && word.chars().all(|c| c.is_ascii_digit())
    {
        word.parse().ok()
    } else {
        None
    }
}

/// Season from `S3` / `SEASON 3` / `SERIES 3`; also returns the token indices
/// consumed so they can be removed from the title.
fn find_season(upper: &[String]) -> Option<(u16, Vec<usize>)> {
    for (i, word) in upper.iter().enumerate() {
        if let Some(rest) = word.strip_prefix('S') {
            if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
                if let Ok(n) = rest.parse() {
                    return Some((n, vec![i]));
                }
            }
        }
    }
    for (i, word) in upper.iter().enumerate() {
        if matches!(word.as_str(), "SEASON" | "SERIES") {
            if let Some(next) = upper.get(i + 1) {
                if let Ok(n) = next.parse() {
                    return Some((n, vec![i, i + 1]));
                }
            }
        }
    }
    None
}

/// Disc from `D2` / `DISC 2` / `DISK 2`.
fn find_disc(upper: &[String]) -> Option<(u16, Vec<usize>)> {
    for (i, word) in upper.iter().enumerate() {
        if let Some(rest) = word.strip_prefix('D') {
            if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
                if let Ok(n) = rest.parse() {
                    return Some((n, vec![i]));
                }
            }
        }
    }
    for (i, word) in upper.iter().enumerate() {
        if matches!(word.as_str(), "DISC" | "DISK") {
            if let Some(next) = upper.get(i + 1) {
                if let Ok(n) = next.parse() {
                    return Some((n, vec![i, i + 1]));
                }
            }
        }
    }
    None
}

/// Words that are packaging noise rather than part of a title.
fn is_noise(upper: &str) -> bool {
    if upper.len() == 4
        && (upper.starts_with("19") || upper.starts_with("20"))
        && upper.chars().all(|c| c.is_ascii_digit())
    {
        return true;
    }
    if upper.len() >= 2 {
        for prefix in ['S', 'D'] {
            if let Some(rest) = upper.strip_prefix(prefix) {
                if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
                    return true;
                }
            }
        }
    }
    matches!(
        upper,
        "SEASON"
            | "SERIES"
            | "DISC"
            | "DISK"
            | "DVD"
            | "VOL"
            | "VOLUME"
            | "PART"
            | "COMPLETE"
            | "REPACK"
            | "RETAIL"
            | "NTSC"
            | "PAL"
            | "BLURAY"
            | "BLU"
            | "RAY"
            | "UHD"
            | "1080P"
            | "720P"
            | "4K"
    )
}

/// Title-case words, keeping small connector words lowercase after the first.
fn title_case(input: &str) -> String {
    const SMALL: &[&str] = &[
        "a", "an", "and", "as", "at", "but", "by", "for", "in", "nor", "of", "on", "or", "per",
        "the", "to", "up", "via", "vs",
    ];
    input
        .split_whitespace()
        .enumerate()
        .map(|(i, word)| {
            let lower = word.to_lowercase();
            if i > 0 && SMALL.contains(&lower.as_str()) {
                lower
            } else {
                let mut chars = lower.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                    None => String::new(),
                }
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_season_and_disc() {
        let info = parse_label("DRAGON_BALL_S1_D1");
        assert_eq!(info.title, "Dragon Ball");
        assert_eq!(info.season, Some(1));
        assert_eq!(info.disc, Some(1));
        assert!(info.looks_like_tv);
    }

    #[test]
    fn parses_movie_year() {
        let info = parse_label("THE_MATRIX_1999");
        assert_eq!(info.title, "The Matrix");
        assert_eq!(info.year, Some(1999));
        assert_eq!(info.season, None);
        assert!(!info.looks_like_tv);
    }

    #[test]
    fn parses_worded_season_disc() {
        let info = parse_label("FIREFLY SEASON 2 DISC 3");
        assert_eq!(info.title, "Firefly");
        assert_eq!(info.season, Some(2));
        assert_eq!(info.disc, Some(3));
    }

    #[test]
    fn keeps_numbered_title_words() {
        // "SE7EN" must not be mistaken for a season.
        let info = parse_label("SE7EN");
        assert_eq!(info.title.to_lowercase(), "se7en");
        assert_eq!(info.season, None);
    }
}
