//! Deciding what is on a disc: movie vs TV, and how a long "Play All" title
//! breaks into episodes.
//!
//! Everything here is heuristic. The goal is a defensible *guess* plus a
//! confidence score, so the UI can prompt only when it is genuinely unsure.
//! Metadata matching (later) supplies the ground truth for episode lengths.

use std::time::Duration;

use crate::disc::{DiscModel, Title};
use crate::identify::parse_label;

/// Titles shorter than this are menus/warnings/previews, not content.
pub const MIN_CONTENT: Duration = Duration::from_secs(5 * 60);
/// Titles at least this long but under [`MIN_CONTENT`] are extras.
pub const EXTRA_MIN: Duration = Duration::from_secs(30);
/// Plausible single-episode length range.
pub const EPISODE_MIN: Duration = Duration::from_secs(15 * 60);
pub const EPISODE_MAX: Duration = Duration::from_secs(45 * 60);
/// A single title this long is a feature film candidate.
pub const MOVIE_MIN: Duration = Duration::from_secs(70 * 60);

/// What the disc looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscKind {
    Movie,
    TvSeries,
    Unknown,
}

/// A movie/TV verdict with a confidence (0..=100) and human-readable reasons.
#[derive(Debug, Clone)]
pub struct Classification {
    pub kind: DiscKind,
    pub confidence: u8,
    pub reasons: Vec<String>,
}

/// One episode-sized slice of a title, expressed in chapters (1-based,
/// inclusive on both ends).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub start_chapter: u16,
    pub end_chapter: u16,
    pub duration: Duration,
}

/// A repeating episode layout inside one title.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChapterPattern {
    /// How many chapters make up one episode.
    pub period: usize,
    /// Chapters before the first full episode (a leading opening/trailer).
    pub offset: usize,
}

/// Classify a disc from title durations, chapter structure and the volume
/// label.
///
/// The label matters: a disc explicitly labelled as a season is television
/// even when its individual titles are feature length, while a release year
/// and no season marker is corroboration for a movie.
pub fn classify(disc: &DiscModel) -> Classification {
    let content = disc.content_titles(MIN_CONTENT);
    if content.is_empty() {
        return Classification {
            kind: DiscKind::Unknown,
            confidence: 0,
            reasons: vec!["no title is long enough to be content".into()],
        };
    }

    let label = parse_label(&disc.volume_id);
    let season_label = label.season.is_some();
    let year_label = label.year.is_some();

    let longest = content
        .iter()
        .max_by_key(|t| t.duration.unwrap_or_default())
        .expect("non-empty");
    let longest_duration = longest.duration.unwrap_or_default();

    // Several independent titles of episode length => a normal episodic disc.
    let episode_like = content
        .iter()
        .filter(|t| is_episode_length(t.duration.unwrap_or_default()))
        .count();
    if episode_like >= 2 {
        let confidence = if season_label { 90 } else { 80 };
        return Classification {
            kind: DiscKind::TvSeries,
            confidence,
            reasons: vec![format!(
                "{episode_like} separate titles are episode length (15-45 min){}",
                label_suffix(season_label)
            )],
        };
    }

    // One dominant long title: a feature film, or a "Play All" season disc.
    if longest_duration >= MOVIE_MIN {
        let segments = split_title(longest);
        if segments.len() >= 3 {
            let average = mean_duration(&segments);
            if is_episode_length(average) {
                let confidence = if season_label { 80 } else { 70 };
                return Classification {
                    kind: DiscKind::TvSeries,
                    confidence,
                    reasons: vec![format!(
                        "longest title ({}) splits into {} episode-length chapter groups (~{} min each){}",
                        fmt(longest_duration),
                        segments.len(),
                        average.as_secs() / 60,
                        label_suffix(season_label)
                    )],
                };
            }
        }

        // A season marker outranks film-length structure: a disc of
        // feature-length episodes is still a season.
        if season_label {
            return Classification {
                kind: DiscKind::TvSeries,
                confidence: 60,
                reasons: vec![format!(
                    "label names a season and the longest title is {}",
                    fmt(longest_duration)
                )],
            };
        }

        let features = feature_titles(disc);
        if features.len() >= 2 {
            let durations: Vec<String> = features
                .iter()
                .map(|t| fmt(t.duration.unwrap_or_default()))
                .collect();
            return Classification {
                kind: DiscKind::Movie,
                confidence: 70,
                reasons: vec![format!(
                    "{} feature-length titles on one disc ({})",
                    features.len(),
                    durations.join(", ")
                )],
            };
        }

        let confidence = if year_label { 75 } else { 65 };
        let mut reasons = vec![format!("one dominant feature of {}", fmt(longest_duration))];
        if year_label {
            reasons.push("label carries a release year and no season marker".into());
        }
        return Classification {
            kind: DiscKind::Movie,
            confidence,
            reasons,
        };
    }

    if longest_duration >= EPISODE_MIN {
        let confidence = if season_label { 60 } else { 45 };
        return Classification {
            kind: DiscKind::TvSeries,
            confidence,
            reasons: vec![format!(
                "longest title is {} (episode length, but not a feature film){}",
                fmt(longest_duration),
                label_suffix(season_label)
            )],
        };
    }

    Classification {
        kind: DiscKind::Unknown,
        confidence: 20,
        reasons: vec!["no movie-length or episode-length title found".into()],
    }
}

/// Feature-length titles on a disc, longest first.
///
/// A double feature, a film plus its long making-of, or a movie split across
/// titles all show up here, so the movie path can plan more than the single
/// longest title.
pub fn feature_titles(disc: &DiscModel) -> Vec<&Title> {
    let mut features = disc.content_titles(MOVIE_MIN);
    features.sort_by_key(|t| std::cmp::Reverse(t.duration.unwrap_or_default()));
    features
}

/// Bonus content on a movie disc: every title except the main feature that is
/// long enough to be intentional (menus and warnings are shorter than
/// [`EXTRA_MIN`]).
///
/// This deliberately includes additional *feature-length* titles as well as
/// short featurettes and trailers, so the selection list can offer everything
/// that might be worth ripping rather than silently dropping it.
pub fn movie_extras(disc: &DiscModel) -> Vec<&Title> {
    let primary = feature_titles(disc).first().map(|t| t.number);
    let mut extras: Vec<&Title> = disc
        .titles
        .iter()
        .filter(|t| Some(t.number) != primary && t.duration.is_some_and(|d| d >= EXTRA_MIN))
        .collect();
    extras.sort_by_key(|t| t.number);
    extras
}

/// `, label names a season` when the volume label carried a season number.
fn label_suffix(season_label: bool) -> &'static str {
    if season_label {
        ", label names a season"
    } else {
        ""
    }
}

/// Split a title into episode-sized chapter groups using the repeating layout
/// most television discs are authored with.
pub fn split_title(title: &Title) -> Vec<Segment> {
    let chapters = &title.chapter_durations;
    let Some(pattern) = chapter_pattern(chapters) else {
        return Vec::new();
    };

    let mut segments = Vec::new();

    // The lead-in chapters belong to the first episode (an opening or a
    // pre-roll that plays before it), so episode one starts at chapter 1.
    let first_end = pattern.offset + pattern.period;
    if first_end <= chapters.len() {
        segments.push(make_segment(0, first_end, chapters));
    }

    let mut start = first_end;
    while start + pattern.period <= chapters.len() {
        segments.push(make_segment(start, start + pattern.period, chapters));
        start += pattern.period;
    }

    // A short tail is usually a trailer/preview, but a nearly-full final group
    // is a real episode whose pattern was cut short by the authoring tool.
    if start < chapters.len() {
        let tail = make_segment(start, chapters.len(), chapters);
        match median_duration(&segments) {
            Some(median) if segments.is_empty() || tail.duration * 2 >= median => {
                segments.push(tail)
            }
            None if segments.is_empty() => segments.push(tail),
            _ => {}
        }
    }

    segments
}

/// A set of titles that look like alternate encodings of the same episodes —
/// for example a "with openings and endings" Play-All and a "clean" one that
/// strips them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlternateSet {
    /// The title number to keep (the longest, which normally retains the
    /// opening/ending).
    pub kept: u16,
    /// Titles that look redundant and are skipped by default.
    pub dropped: Vec<u16>,
    /// Number of episodes each member splits into.
    pub episodes: usize,
}

/// Find groups of alternate Play-All titles.
///
/// Two titles are treated as alternates when they split into the same number
/// of episodes and their total runtimes are within 25% of each other. On a
/// real TV disc that is exactly the "with OP/ED" vs "without OP/ED" pair; the
/// margin is loose on purpose, with metadata matching as the final arbiter.
pub fn alternates(disc: &DiscModel) -> Vec<AlternateSet> {
    let mut summaries: Vec<(u16, Duration, usize)> = Vec::new();
    for title in &disc.titles {
        if title.duration.map_or(true, |d| d < MIN_CONTENT) {
            continue;
        }
        let segments = split_title(title);
        if segments.len() >= 3 {
            summaries.push((
                title.number,
                title.duration.unwrap_or_default(),
                segments.len(),
            ));
        }
    }

    let mut groups: Vec<Vec<u16>> = Vec::new();
    for i in 0..summaries.len() {
        for j in (i + 1)..summaries.len() {
            let (a_number, a_total, a_episodes) = summaries[i];
            let (b_number, b_total, b_episodes) = summaries[j];
            if a_episodes != b_episodes {
                continue;
            }
            let largest = a_total.max(b_total);
            if largest.is_zero() {
                continue;
            }
            let difference = largest - a_total.min(b_total);
            if difference * 100 > largest * 25 {
                continue;
            }
            add_pair(&mut groups, a_number, b_number);
        }
    }

    let mut out = Vec::new();
    for group in groups {
        let mut members: Vec<(u16, Duration, usize)> = group
            .iter()
            .filter_map(|n| summaries.iter().find(|(num, _, _)| num == n).copied())
            .collect();
        if members.len() < 2 {
            continue;
        }
        // Longest total wins; ties break on the lower title number.
        members.sort_by_key(|(number, total, _)| (*total, *number));
        let (kept, _, episodes) = *members.last().expect("non-empty");
        let dropped = members
            .iter()
            .rev()
            .skip(1)
            .map(|(number, _, _)| *number)
            .collect();
        out.push(AlternateSet {
            kept,
            dropped,
            episodes,
        });
    }
    out
}

/// Title numbers to rip by default: content titles with redundant alternates
/// removed.
pub fn preferred_titles(disc: &DiscModel) -> Vec<u16> {
    let dropped: std::collections::BTreeSet<u16> = alternates(disc)
        .into_iter()
        .flat_map(|set| set.dropped)
        .collect();
    disc.titles
        .iter()
        .filter(|t| t.duration.is_some_and(|d| d >= MIN_CONTENT) && !dropped.contains(&t.number))
        .map(|t| t.number)
        .collect()
}

/// Merge `a` and `b` into one group, joining two existing groups if needed.
fn add_pair(groups: &mut Vec<Vec<u16>>, a: u16, b: u16) {
    let ai = groups.iter().position(|g| g.contains(&a));
    let bi = groups.iter().position(|g| g.contains(&b));
    match (ai, bi) {
        (Some(i), Some(j)) if i == j => {}
        (Some(i), Some(j)) => {
            let (keep, other) = if i < j { (i, j) } else { (j, i) };
            let moved: Vec<u16> = groups[other]
                .iter()
                .copied()
                .filter(|n| !groups[keep].contains(n))
                .collect();
            groups[keep].extend(moved);
            groups.remove(other);
        }
        (Some(i), None) => groups[i].push(b),
        (None, Some(j)) => groups[j].push(a),
        (None, None) => groups.push(vec![a, b]),
    }
}

/// Find the repeating episode layout of a chapter-duration sequence.
///
/// Episodes on a disc are not identical chapter-for-chapter, but the *sum* of
/// the chapters in one episode is nearly constant (opening + recap + body +
/// ending). So we look for the smallest block size and lead-in whose
/// consecutive block sums agree, and whose blocks actually have internal
/// structure (a flat chapter grid is not episode structure).
pub fn chapter_pattern(chapters: &[Duration]) -> Option<ChapterPattern> {
    let n = chapters.len();
    if n < 4 {
        return None;
    }

    // (blocks, period, offset, variation)
    let mut best: Option<(usize, usize, usize, f64)> = None;

    for period in 2..=n / 2 {
        if n / period < 2 {
            continue;
        }
        // A flat chapter grid is not a repeated episode signature.
        if block_variation(chapters, period) < 0.15 {
            continue;
        }

        for offset in 0..period {
            let mut sums = Vec::new();
            let mut i = offset;
            while i + period <= n {
                sums.push(
                    chapters[i..i + period]
                        .iter()
                        .map(|d| d.as_secs_f64())
                        .sum::<f64>(),
                );
                i += period;
            }
            if sums.len() < 2 {
                continue;
            }
            let max = sums.iter().copied().fold(0.0f64, f64::max).max(1.0);
            let min = sums.iter().copied().fold(f64::INFINITY, f64::min);
            let variation = (max - min) / max;
            if variation > 0.05 {
                continue;
            }

            let blocks = sums.len();
            let better = match best {
                None => true,
                // Prefer the split that covers the most episodes, then the
                // tightest match.
                Some((best_blocks, _, _, best_variation)) => {
                    blocks > best_blocks || (blocks == best_blocks && variation < best_variation)
                }
            };
            if better {
                best = Some((blocks, period, offset, variation));
            }
        }
    }

    best.map(|(_, period, offset, _)| ChapterPattern { period, offset })
}

/// Mean relative spread `(max-min)/max` inside each block of `period`
/// chapters. Near zero means a flat chapter grid, not repeated episodes.
fn block_variation(chapters: &[Duration], period: usize) -> f64 {
    let mut total = 0.0f64;
    let mut blocks = 0usize;
    let mut start = 0usize;
    while start + period <= chapters.len() {
        let block = &chapters[start..start + period];
        let max = block
            .iter()
            .map(|d| d.as_secs_f64())
            .fold(0.0f64, f64::max)
            .max(1.0);
        let min = block
            .iter()
            .map(|d| d.as_secs_f64())
            .fold(f64::INFINITY, f64::min);
        total += (max - min) / max;
        blocks += 1;
        start += period;
    }
    if blocks == 0 {
        0.0
    } else {
        total / blocks as f64
    }
}

fn make_segment(start: usize, end: usize, chapters: &[Duration]) -> Segment {
    let duration = chapters[start..end]
        .iter()
        .copied()
        .fold(Duration::ZERO, |a, b| a + b);
    Segment {
        start_chapter: (start + 1) as u16,
        end_chapter: end as u16,
        duration,
    }
}

fn mean_duration(segments: &[Segment]) -> Duration {
    if segments.is_empty() {
        return Duration::ZERO;
    }
    let total: Duration = segments.iter().map(|s| s.duration).sum();
    total / segments.len() as u32
}

fn median_duration(segments: &[Segment]) -> Option<Duration> {
    if segments.is_empty() {
        return None;
    }
    let mut values: Vec<Duration> = segments.iter().map(|s| s.duration).collect();
    values.sort_unstable();
    Some(values[values.len() / 2])
}

fn is_episode_length(d: Duration) -> bool {
    (EPISODE_MIN..=EPISODE_MAX).contains(&d)
}

fn fmt(d: Duration) -> String {
    let secs = d.as_secs();
    format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disc::Title;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn title_with(chapters: Vec<Duration>) -> Title {
        Title {
            number: 1,
            vts: 1,
            vts_ttn: 1,
            angles: 1,
            chapters: chapters.len() as u16,
            duration: Some(chapters.iter().copied().sum()),
            chapter_durations: chapters,
        }
    }

    /// Real anime pattern: `[opening, recap, body, body, ending]`, seven
    /// episodes plus a short preview tail.
    #[test]
    fn finds_five_chapter_episodes() {
        let mut chapters = Vec::new();
        for (body_a, body_b) in [(572, 629), (589, 602), (572, 601), (484, 661)] {
            chapters.extend([secs(110), secs(50), secs(body_a), secs(body_b), secs(98)]);
        }
        chapters.extend([secs(10), secs(0)]);

        let pattern = chapter_pattern(&chapters).expect("period");
        assert_eq!(pattern.period, 5);
        assert_eq!(pattern.offset, 0);

        let segments = split_title(&title_with(chapters));
        assert_eq!(segments.len(), 4);
        assert_eq!((segments[0].start_chapter, segments[0].end_chapter), (1, 5));
        assert_eq!(
            (segments[3].start_chapter, segments[3].end_chapter),
            (16, 20)
        );
    }

    /// Real second title on the same disc: three chapters per episode, with a
    /// one-chapter lead-in, and a trailing filler chapter.
    #[test]
    fn finds_three_chapter_episodes_with_lead_in() {
        let chapters: Vec<Duration> = [
            110, // lead-in
            40, 572, 629, // episode 1
            50, 589, 602, // episode 2
            68, 572, 601, // episode 3
            96, 484, 661, // episode 4
            90, 544, 607, // episode 5
            109, 457, 675, // episode 6
            84, 521, 636, // episode 7
            0,   // filler
        ]
        .into_iter()
        .map(secs)
        .collect();

        let pattern = chapter_pattern(&chapters).expect("period");
        assert_eq!(pattern.period, 3);
        assert_eq!(pattern.offset, 1);

        let segments = split_title(&title_with(chapters));
        assert_eq!(segments.len(), 7);
        // Episode 1 absorbs the lead-in.
        assert_eq!((segments[0].start_chapter, segments[0].end_chapter), (1, 4));
        assert_eq!(segments[0].duration, secs(1351));
        assert_eq!(
            (segments[6].start_chapter, segments[6].end_chapter),
            (20, 22)
        );
        assert_eq!(segments[6].duration, secs(1241));
    }

    #[test]
    fn flat_chapter_grid_has_no_pattern() {
        let chapters: Vec<Duration> = (0..12).map(|_| secs(500)).collect();
        assert!(chapter_pattern(&chapters).is_none());
    }

    #[test]
    fn unrelated_chapters_have_no_pattern() {
        let chapters: Vec<Duration> = [100u64, 300, 50, 900, 20].into_iter().map(secs).collect();
        assert!(chapter_pattern(&chapters).is_none());
    }

    #[test]
    fn movie_without_pattern_is_a_movie() {
        let disc = DiscModel {
            volume_id: "TEST".into(),
            provider_id: String::new(),
            vts_count: 1,
            titles: vec![title_with((0..12).map(|_| secs(500)).collect())],
        };
        assert_eq!(classify(&disc).kind, DiscKind::Movie);
    }

    #[test]
    fn two_episode_titles_are_tv() {
        let titles = [1u16, 2]
            .into_iter()
            .map(|n| Title {
                number: n,
                vts: 1,
                vts_ttn: n as u8,
                angles: 1,
                chapters: 1,
                duration: Some(secs(24 * 60)),
                chapter_durations: vec![secs(24 * 60)],
            })
            .collect();
        let disc = DiscModel {
            volume_id: "TEST".into(),
            provider_id: String::new(),
            vts_count: 1,
            titles,
        };
        assert_eq!(classify(&disc).kind, DiscKind::TvSeries);
    }

    fn block_title(number: u16, block: &[u64], repeats: usize) -> Title {
        let mut chapters = Vec::new();
        for _ in 0..repeats {
            chapters.extend(block.iter().copied().map(secs));
        }
        Title {
            number,
            vts: 1,
            vts_ttn: number as u8,
            angles: 1,
            chapters: chapters.len() as u16,
            duration: Some(chapters.iter().copied().sum()),
            chapter_durations: chapters,
        }
    }

    #[test]
    fn detects_alternate_play_all_titles() {
        // 3 episodes with an opening/ending, vs the same 3 without them.
        let with_credits = block_title(1, &[110, 200, 300, 98], 3);
        let without = block_title(2, &[110, 200, 300, 50], 3);
        let disc = DiscModel {
            volume_id: "TEST".into(),
            provider_id: String::new(),
            vts_count: 1,
            titles: vec![with_credits, without],
        };

        let sets = alternates(&disc);
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].kept, 1);
        assert_eq!(sets[0].dropped, vec![2]);
        assert_eq!(sets[0].episodes, 3);
        assert_eq!(preferred_titles(&disc), vec![1]);
    }

    #[test]
    fn different_episode_counts_are_not_alternates() {
        let a = block_title(1, &[110, 200, 300, 98], 3);
        let b = block_title(2, &[110, 200, 300, 98], 4);
        let disc = DiscModel {
            volume_id: "T".into(),
            provider_id: String::new(),
            vts_count: 1,
            titles: vec![a, b],
        };
        assert!(alternates(&disc).is_empty());
    }

    #[test]
    fn season_label_outranks_film_length_structure() {
        let disc = DiscModel {
            volume_id: "SOME_SHOW_S2".into(),
            provider_id: String::new(),
            vts_count: 1,
            titles: vec![title_with((0..12).map(|_| secs(500)).collect())],
        };
        let result = classify(&disc);
        assert_eq!(result.kind, DiscKind::TvSeries);
        assert!(
            result
                .reasons
                .iter()
                .any(|r| r.contains("label names a season")),
            "{:?}",
            result.reasons
        );
    }

    #[test]
    fn year_label_corroborates_a_movie() {
        let disc = DiscModel {
            volume_id: "THE_MATRIX_1999".into(),
            provider_id: String::new(),
            vts_count: 1,
            titles: vec![title_with((0..12).map(|_| secs(500)).collect())],
        };
        let result = classify(&disc);
        assert_eq!(result.kind, DiscKind::Movie);
        assert_eq!(result.confidence, 75);
    }

    #[test]
    fn two_features_are_a_movie_collection() {
        let disc = DiscModel {
            volume_id: "DOUBLE_FEATURE".into(),
            provider_id: String::new(),
            vts_count: 1,
            titles: vec![
                block_title(1, &[600; 12], 1), // 120 min
                block_title(2, &[540; 12], 1), // 108 min
            ],
        };
        let result = classify(&disc);
        assert_eq!(result.kind, DiscKind::Movie);
        assert!(
            result
                .reasons
                .iter()
                .any(|r| r.contains("2 feature-length titles")),
            "{:?}",
            result.reasons
        );
    }

    #[test]
    fn feature_titles_are_longest_first() {
        let disc = DiscModel {
            volume_id: "DOUBLE_FEATURE".into(),
            provider_id: String::new(),
            vts_count: 1,
            titles: vec![
                block_title(1, &[540; 12], 1), // 108 min
                block_title(2, &[600; 12], 1), // 120 min
            ],
        };
        let numbers: Vec<u16> = feature_titles(&disc).iter().map(|t| t.number).collect();
        assert_eq!(numbers, vec![2, 1]);
    }

    #[test]
    fn movie_extras_cover_bonus_and_second_features() {
        // Title 1 is the feature, title 2 a second feature, title 3 a 20-minute
        // making-of, title 4 a 3-minute trailer and title 5 a menu sting.
        let disc = DiscModel {
            volume_id: "DOUBLE_FEATURE".into(),
            provider_id: String::new(),
            vts_count: 1,
            titles: vec![
                title_with((0..12).map(|_| secs(600)).collect()), // 120 min
                block_title(2, &[600; 12], 1),                    // 120 min
                block_title(3, &[600; 2], 1),                     // 20 min
                block_title(4, &[180], 1),                        // 3 min
                block_title(5, &[10], 1),                         // 10 s menu
            ],
        };
        let numbers: Vec<u16> = movie_extras(&disc).iter().map(|t| t.number).collect();
        // The longest stays the feature even though it was listed first.
        assert_eq!(numbers, vec![2, 3, 4]);
        assert_eq!(feature_titles(&disc).first().unwrap().number, 1);
    }
}
