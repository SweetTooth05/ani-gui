use crate::model::Progress;

const COMPLETED_FRACTION: f64 = 0.9;

pub fn is_completed(p: &Progress) -> bool {
    p.duration > 0.0 && p.position / p.duration >= COMPLETED_FRACTION
}

/// Where Continue Watching picks up: the next episode from the start once
/// `latest` is completed (None after the last one), else `latest` at its
/// position.
pub fn continue_point(latest: &Progress, episodes: &[String]) -> Option<(String, f64)> {
    if is_completed(latest) {
        next_after(&latest.ep, episodes).map(|ep| (ep, 0.0))
    } else {
        Some((latest.ep.clone(), latest.position))
    }
}

/// The episode after the one ani-cli's history says was last watched.
pub fn history_continue(entry_ep: &str, episodes: &[String]) -> Option<String> {
    next_after(entry_ep, episodes)
}

/// The episode following `ep` in `episodes` (sorted as AllAnime lists them).
/// Labels compare numerically when both parse ("05" is "5"); an `ep` missing
/// from the list continues at the first numerically higher episode.
fn next_after(ep: &str, episodes: &[String]) -> Option<String> {
    let num = |e: &str| e.parse::<f64>().ok().filter(|n| n.is_finite());
    let target = num(ep);
    let same = |e: &str| e == ep || (target.is_some() && num(e) == target);
    match episodes.iter().position(|e| same(e)) {
        Some(i) => episodes.get(i + 1).cloned(),
        None => {
            let target = target?;
            episodes
                .iter()
                .find(|e| num(e).is_some_and(|n| n > target))
                .cloned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Mode;

    fn eps(list: &[&str]) -> Vec<String> {
        list.iter().map(|e| e.to_string()).collect()
    }

    fn progress(ep: &str, position: f64, duration: f64) -> Progress {
        Progress {
            show_id: "s".into(),
            mode: Mode::Sub,
            ep: ep.into(),
            position,
            duration,
            updated_at: 0,
        }
    }

    #[test]
    fn is_completed_at_ninety_percent() {
        assert!(is_completed(&progress("1", 1260.0, 1400.0)));
        assert!(!is_completed(&progress("1", 1259.0, 1400.0)));
        assert!(!is_completed(&progress("1", 10.0, 0.0)));
    }

    #[test]
    fn continue_point_completed_moves_next() {
        let list = eps(&["1", "2", "5.5", "10"]);
        assert_eq!(
            continue_point(&progress("2", 1390.0, 1400.0), &list),
            Some(("5.5".into(), 0.0))
        );
        // Matched numerically when the stored label is zero-padded.
        assert_eq!(
            continue_point(&progress("05.5", 1400.0, 1400.0), &list),
            Some(("10".into(), 0.0))
        );
        // A completed episode missing from the list continues at the next higher one.
        assert_eq!(
            continue_point(&progress("3", 1400.0, 1400.0), &list),
            Some(("5.5".into(), 0.0))
        );
    }

    #[test]
    fn continue_point_partial_resumes() {
        let list = eps(&["1", "2", "3"]);
        assert_eq!(
            continue_point(&progress("2", 312.5, 1400.0), &list),
            Some(("2".into(), 312.5))
        );
        // Unknown duration (never loaded) is not completed.
        assert_eq!(
            continue_point(&progress("3", 0.0, 0.0), &list),
            Some(("3".into(), 0.0))
        );
    }

    #[test]
    fn continue_point_last_episode_none() {
        let list = eps(&["1", "2", "3"]);
        assert_eq!(continue_point(&progress("3", 1350.0, 1400.0), &list), None);
        assert_eq!(continue_point(&progress("1", 1400.0, 1400.0), &[]), None);
    }

    #[test]
    fn history_continue_next_after_entry() {
        let list = eps(&["1", "2", "3"]);
        assert_eq!(history_continue("1", &list), Some("2".into()));
        assert_eq!(history_continue("3", &list), None);
        assert_eq!(history_continue("x", &list), None);
    }
}
