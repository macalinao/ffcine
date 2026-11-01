//! Full-text search over transcripts: `ffcine search "konbini"` -> clip + timecode.
//!
//! Words are folded (lowercase, accents and punctuation dropped) and a query matches a run of
//! consecutive words, each query word a prefix of its transcript word ("konbini" finds
//! "konbini's"). Clips without word timing fall back to segment text.

use crate::index::{Clip, Index, Segment};

#[derive(Debug, Clone, PartialEq)]
pub struct Hit<'a> {
    pub clip: &'a Clip,
    /// Seconds into the clip.
    pub start: f64,
    pub end: f64,
    /// The segment around the match.
    pub context: String,
}

/// Lowercase, strip accents (Latin-1 and common Latin Extended-A), keep letters/digits and
/// non-Latin scripts as they are.
pub fn fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars().flat_map(char::to_lowercase) {
        let c = match c {
            'à'..='å' | 'ā' | 'ă' | 'ą' => 'a',
            'ç' | 'ć' | 'č' => 'c',
            'è'..='ë' | 'ē' | 'ė' | 'ę' | 'ě' => 'e',
            'ì'..='ï' | 'ī' | 'į' => 'i',
            'ñ' | 'ń' => 'n',
            'ò'..='ö' | 'ø' | 'ō' | 'ő' => 'o',
            'ù'..='ü' | 'ū' | 'ů' | 'ű' => 'u',
            'ý' | 'ÿ' => 'y',
            c => c,
        };
        if c.is_alphanumeric() {
            out.push(c);
        } else if c.is_whitespace() || c == '-' {
            out.push(' ');
        }
    }
    out
}

fn tokens(s: &str) -> Vec<String> {
    fold(s).split_whitespace().map(str::to_owned).collect()
}

/// Words the crew says that whisper writes differently. A query equal to a key also searches its
/// values.
pub const ALIASES: &[(&str, &[&str])] = &[(
    "konbini",
    &[
        "convenience store",
        "family mart",
        "familymart",
        "lawson",
        "seven eleven",
        "7 eleven",
        "combini",
        "conbini",
    ],
)];

/// The query plus its aliases.
pub fn expand(query: &str) -> Vec<String> {
    let q = tokens(query).join(" ");
    let mut out = vec![q.clone()];
    for (k, v) in ALIASES {
        if q == *k {
            out.extend(v.iter().map(|s| (*s).to_owned()));
        }
    }
    out
}

/// [`search_exact`] over the query and its [`expand`]ed aliases, by clip then time.
pub fn search<'a>(index: &'a Index, query: &str) -> Vec<Hit<'a>> {
    let mut hits: Vec<Hit<'a>> = expand(query)
        .iter()
        .flat_map(|q| search_exact(index, q))
        .collect();
    hits.sort_by(|a, b| a.clip.id.cmp(&b.clip.id).then(a.start.total_cmp(&b.start)));
    hits.dedup_by(|a, b| a.clip.id == b.clip.id && (a.start - b.start).abs() < 1e-6);
    hits
}

pub fn search_exact<'a>(index: &'a Index, query: &str) -> Vec<Hit<'a>> {
    let q = tokens(query);
    let mut hits = Vec::new();
    if q.is_empty() {
        return hits;
    }
    for clip in &index.clips {
        let Some(t) = &clip.transcript else { continue };
        for seg in &t.segments {
            if seg.words.is_empty() {
                if let Some(h) = segment_hit(clip, seg, &q) {
                    hits.push(h);
                }
                continue;
            }
            // A word may fold to several tokens ("ni-hao" -> "ni", "hao"): keep each token's word.
            let toks: Vec<(String, usize)> = seg
                .words
                .iter()
                .enumerate()
                .flat_map(|(i, w)| tokens(&w.text).into_iter().map(move |t| (t, i)))
                .collect();
            let mut i = 0;
            while i + q.len() <= toks.len() {
                if q.iter()
                    .zip(&toks[i..])
                    .all(|(q, (t, _))| t.starts_with(q.as_str()))
                {
                    let (a, b) = (toks[i].1, toks[i + q.len() - 1].1);
                    hits.push(Hit {
                        clip,
                        start: seg.words[a].start,
                        end: seg.words[b].end,
                        context: seg.text.trim().to_owned(),
                    });
                    i += q.len();
                } else {
                    i += 1;
                }
            }
        }
    }
    hits
}

fn segment_hit<'a>(clip: &'a Clip, seg: &Segment, q: &[String]) -> Option<Hit<'a>> {
    let toks = tokens(&seg.text);
    toks.windows(q.len())
        .any(|w| w.iter().zip(q).all(|(t, q)| t.starts_with(q.as_str())))
        .then(|| Hit {
            clip,
            start: seg.start,
            end: seg.end,
            context: seg.text.trim().to_owned(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{Transcript, Word};

    fn w(start: f64, text: &str) -> Word {
        Word {
            start,
            end: start + 0.3,
            text: text.into(),
            prob: 1.0,
        }
    }

    fn index() -> Index {
        let seg = Segment {
            start: 10.0,
            end: 12.0,
            text: " Let's hit the Konbini's coffee.".into(),
            words: vec![
                w(10.0, " Let's"),
                w(10.3, " hit"),
                w(10.6, " the"),
                w(10.9, " Konbini's"),
                w(11.2, " coffee."),
            ],
        };
        let plain = Segment {
            start: 20.0,
            end: 22.0,
            text: "Café au lait".into(),
            words: vec![],
        };
        Index {
            clips: vec![Clip {
                id: "c1".into(),
                transcript: Some(Transcript {
                    segments: vec![seg, plain],
                    ..Transcript::default()
                }),
                ..Clip::default()
            }],
            ..Index::default()
        }
    }

    #[test]
    fn finds_words_and_phrases() {
        let ix = index();
        let h = search(&ix, "konbini");
        assert_eq!(h.len(), 1);
        assert!((h[0].start - 10.9).abs() < 1e-9);
        let h = search(&ix, "the KONBINI coffee");
        assert_eq!(h.len(), 1);
        assert!((h[0].end - 11.5).abs() < 1e-9);
        assert_eq!(search(&ix, "coffee the"), []);
        assert_eq!(search(&ix, "cafe").len(), 1);
        assert!(expand("Konbini").contains(&"family mart".to_owned()));
    }
}
