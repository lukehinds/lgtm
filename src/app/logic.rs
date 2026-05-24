use super::CacheFilter;

pub(super) fn item_visible(
    cached: bool,
    filter: CacheFilter,
    query: &str,
    fields: &[&str],
) -> bool {
    let cache_ok = match filter {
        CacheFilter::All => true,
        CacheFilter::Cached => cached,
        CacheFilter::Uncached => !cached,
    };
    if !cache_ok {
        return false;
    }
    let query = query.trim().to_lowercase();
    query.is_empty()
        || fields
            .iter()
            .any(|field| field.to_lowercase().contains(&query))
}

pub(super) fn find_line_after(text: &str, query: &str, start: usize) -> Option<usize> {
    text.lines()
        .enumerate()
        .skip(start)
        .find(|(_, line)| line.to_lowercase().contains(query))
        .map(|(index, _)| index)
}

pub(super) fn offset_index(current: usize, delta: isize, max: usize) -> usize {
    if delta.is_negative() {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        current.saturating_add(delta as usize).min(max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_visible_honors_cache_filter() {
        assert!(item_visible(true, CacheFilter::Cached, "", &["title"]));
        assert!(!item_visible(false, CacheFilter::Cached, "", &["title"]));
        assert!(item_visible(false, CacheFilter::Uncached, "", &["title"]));
        assert!(!item_visible(true, CacheFilter::Uncached, "", &["title"]));
    }

    #[test]
    fn item_visible_searches_case_insensitively() {
        assert!(item_visible(
            false,
            CacheFilter::All,
            "AUTH",
            &["title", "author-name"]
        ));
        assert!(!item_visible(
            false,
            CacheFilter::All,
            "missing",
            &["title", "author-name"]
        ));
    }

    #[test]
    fn find_line_after_returns_matching_line_from_start() {
        let text = "alpha\nbeta\ngamma\nbeta";
        assert_eq!(find_line_after(text, "beta", 0), Some(1));
        assert_eq!(find_line_after(text, "beta", 2), Some(3));
        assert_eq!(find_line_after(text, "delta", 0), None);
    }

    #[test]
    fn offset_index_clamps_to_bounds() {
        assert_eq!(offset_index(2, 1, 4), 3);
        assert_eq!(offset_index(2, 10, 4), 4);
        assert_eq!(offset_index(2, -1, 4), 1);
        assert_eq!(offset_index(2, -10, 4), 0);
    }
}
