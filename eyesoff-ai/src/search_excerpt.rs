//! Bounded, verbatim search excerpts. This is selection, not summarization:
//! no model, credentials, additional network requests, or synthesized facts.
use std::collections::BTreeSet;

/// Max-min allocation: preserve short snippets while giving every long source
/// a share. A zero configured limit explicitly retains the old full-text mode.
pub fn budgets(lengths: &[usize], limit: usize) -> Vec<usize> {
    if limit == 0 || lengths.iter().copied().sum::<usize>() <= limit {
        return lengths.to_vec();
    }
    let mut order: Vec<usize> = (0..lengths.len()).collect();
    order.sort_by_key(|&i| (lengths[i], i));
    let mut out = vec![0; lengths.len()];
    let mut left = limit;
    for (n, &i) in order.iter().enumerate() {
        let share = left / (order.len() - n);
        out[i] = lengths[i].min(share);
        left -= out[i];
    }
    out
}

fn terms(s: &str) -> BTreeSet<String> {
    const STOP: &[&str] = &["the", "and", "for", "that", "this", "with", "from", "what",
        "which", "how", "why", "does", "are", "was", "were", "have", "has", "about",
        "into", "can", "could", "would", "should", "please", "search", "find", "web"];
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 3)
        .map(str::to_lowercase)
        .filter(|w| !STOP.contains(&w.as_str()))
        .take(128).collect()
}

/// Divide into readable spans, preferring line/sentence endings. Long code or
/// CJK runs still have a bound; indices always originate at UTF-8 boundaries.
fn passages(text: &str) -> Vec<&str> {
    let mut spans = Vec::new();
    let mut start = 0;
    let mut chars = 0;
    for (i, c) in text.char_indices() {
        chars += 1;
        if chars >= 700 || (chars >= 200 && (c == '\n' || matches!(c, '.' | '!' | '?' | '。' | '！' | '？'))) {
            let end = i + c.len_utf8();
            spans.push(&text[start..end]);
            start = end;
            chars = 0;
        }
    }
    if start < text.len() { spans.push(&text[start..]); }
    spans
}

const GAP: &str = "\n[…]\n";

/// Choose high-overlap passages, then restore document order. All selected
/// bytes come from the source, omissions are marked, and the character budget
/// includes those markers. Short pages are returned byte-for-byte unchanged.
pub fn select(query: &str, text: &str, budget: usize) -> String {
    if text.chars().count() <= budget { return text.to_string(); }
    if budget == 0 { return String::new(); }
    let spans = passages(text);
    let query = terms(query);
    let matched: Vec<BTreeSet<String>> = spans.iter()
        .map(|s| terms(s).intersection(&query).cloned().collect()).collect();
    // Rare query terms distinguish the answer-bearing paragraph from a page
    // whose navigation repeats its general subject in every section.
    let weight = |term: &str| -> usize {
        1024 / matched.iter().filter(|m| m.contains(term)).count().max(1)
    };
    let mut ranked: Vec<usize> = (0..spans.len()).collect();
    ranked.sort_by_key(|&i| (
        std::cmp::Reverse(matched[i].iter().map(|t| weight(t)).sum::<usize>()), i));
    let gap_len = GAP.chars().count();
    let mut left = budget.saturating_sub(gap_len);
    let mut selected: Vec<(usize, &str)> = Vec::new();
    for i in ranked {
        // Reserve one omission marker per chosen passage, including a final
        // cut. Prefer whole spans; only the best span may be cut to fit.
        let n = spans[i].chars().count();
        if n + gap_len <= left {
            selected.push((i, spans[i]));
            left -= n + gap_len;
        } else if selected.is_empty() {
            let take = left.saturating_sub(gap_len);
            let end = spans[i].char_indices().nth(take).map_or(spans[i].len(), |(p, _)| p);
            selected.push((i, &spans[i][..end]));
            break;
        }
    }
    selected.sort_by_key(|(i, _)| *i);
    let mut out = String::new();
    if selected.first().is_some_and(|(i, _)| *i > 0) && gap_len <= budget { out.push_str(GAP); }
    for (n, (i, span)) in selected.iter().enumerate() {
        if n > 0 && *i != selected[n - 1].0 + 1 { out.push_str(GAP); }
        out.push_str(span);
    }
    // The caller labels this partial even if the final selected span is at
    // the end: an earlier omitted section must not look like a full page.
    if out.chars().count() + gap_len <= budget { out.push_str(GAP); }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocation_retains_every_source_and_returns_unused_snippet_budget() {
        assert_eq!(budgets(&[6000, 6000, 6000, 200, 200, 200], 6000),
                   vec![1800, 1800, 1800, 200, 200, 200]);
        assert_eq!(budgets(&[6000, 12], 0), vec![6000, 12]);
        assert_eq!(budgets(&[20, 30], 100), vec![20, 30]);
        assert_eq!(budgets(&[100; 10], 3).iter().sum::<usize>(), 3);
        assert!(budgets(&[], 6000).is_empty());
    }

    #[test]
    fn keeps_late_answer_instead_of_only_page_opening() {
        let intro = "Navigation and general background for visitors. ".repeat(75);
        let fact = "The orbit period is 27.3 days. The synodic month lasts 29.5 days because Earth also orbits the Sun. ";
        let text = format!("{intro}\n{fact}{}", "More navigation and general background. ".repeat(50));
        let out = select("Moon synodic month lasts how many days", &text, 1600);
        assert!(out.contains(fact), "{out}");
        assert!(out.chars().count() <= 1600);
        assert!(out.contains("[…]"));
    }

    #[test]
    fn short_text_is_exact_and_long_excerpts_are_verbatim() {
        let text = "First line.\nSecond line with 12.5% and a caveat: only at room temperature.";
        assert_eq!(select("temperature", text, 500), text);
        let long = format!("{}\n{text}\n{}", "unrelated introductory text ".repeat(100), "other text ".repeat(100));
        let out = select("12.5 temperature caveat", &long, 1800);
        assert!(out.contains("only at room temperature"));
        for part in out.split(GAP).filter(|s| !s.is_empty()) { assert!(long.contains(part)); }
    }

    #[test]
    fn unicode_and_tiny_budgets_are_bounded() {
        let text = "🌒 月亮的相位取决于太阳照亮的部分。\n".repeat(100);
        for n in [0, 1, 4, 5, 6, 20, 150, 700, 1200] {
            let out = select("月亮 相位", &text, n);
            assert!(out.chars().count() <= n, "budget {n}");
        }
    }

    #[test]
    fn no_matching_query_is_deterministic_and_keeps_source_order() {
        let text = (0..80).map(|i| format!("Paragraph {i}: unrelated factual background material.\n")).collect::<String>();
        let out = select("xyzmissing", &text, 1000);
        assert!(out.starts_with("Paragraph 0:"));
        assert_eq!(out, select("xyzmissing", &text, 1000));
    }
}
