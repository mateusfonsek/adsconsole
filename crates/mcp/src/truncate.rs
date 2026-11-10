//! Generic response truncation: keeps the longest prefix of items whose JSON fits.

use serde::Serialize;
use serde_json::Value;

/// JSON byte limit of a read response.
pub const MAX_RESPONSE_BYTES: usize = 100_000;

/// Budget for the item list: the limit minus some slack for the rest of the response
/// (envelope keys, `next_cursor`, `quota_warning`).
pub const ITEMS_BUDGET: usize = MAX_RESPONSE_BYTES - 2_000;

/// Keeps the longest prefix of `items` whose JSON array fits in `max_bytes`, with at least
/// one item (a single item larger than the limit is returned anyway). The boolean is
/// `true` when some item was left out.
pub fn fit(items: Vec<Value>, max_bytes: usize) -> (Vec<Value>, bool) {
    let total = items.len();
    let mut size = 2; // "[]"
    let mut keep = 0;
    for item in &items {
        let len = serde_json::to_vec(item).map(|b| b.len()).unwrap_or(0);
        let next = size + len + usize::from(keep > 0);
        if keep > 0 && next > max_bytes {
            break;
        }
        size = next;
        keep += 1;
    }
    let mut items = items;
    items.truncate(keep);
    (items, keep < total)
}

/// Like [`fit`], for a typed list: cuts `items` in place and says whether it cut.
pub fn fit_items<T: Serialize>(items: &mut Vec<T>, max_bytes: usize) -> bool {
    let values: Vec<Value> = items
        .iter()
        .map(|i| serde_json::to_value(i).unwrap_or(Value::Null))
        .collect();
    let (kept, truncated) = fit(values, max_bytes);
    items.truncate(kept.len());
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(n: usize) -> Value {
        json!({ "pad": "x".repeat(30_000), "n": n })
    }

    #[test]
    fn fit_keeps_prefix() {
        let items: Vec<Value> = (0..10).map(item).collect();
        let (kept, truncated) = fit(items, MAX_RESPONSE_BYTES);
        assert_eq!(kept.len(), 3);
        assert!(truncated);
        assert_eq!(kept[2]["n"], 2);
        let small = vec![json!(1), json!(2)];
        assert_eq!(fit(small.clone(), MAX_RESPONSE_BYTES), (small, false));
    }

    #[test]
    fn fit_keeps_at_least_one_and_handles_empty() {
        let (kept, truncated) = fit(vec![item(0), item(1)], 10);
        assert_eq!(kept.len(), 1);
        assert!(truncated);
        assert_eq!(fit(vec![], 10), (vec![], false));
    }

    #[test]
    fn fit_counts_brackets_and_commas() {
        // "[1,2]" is 5 bytes.
        let v = vec![json!(1), json!(2), json!(3)];
        assert_eq!(fit(v.clone(), 5).0.len(), 2);
        assert_eq!(fit(v.clone(), 4).0.len(), 1);
        assert_eq!(fit(v, 7), (vec![json!(1), json!(2), json!(3)], false));
    }

    #[test]
    fn fit_items_cuts_in_place() {
        let mut v: Vec<u32> = (0..10).collect();
        assert!(fit_items(&mut v, 10));
        assert_eq!(v, vec![0, 1, 2, 3]);
    }
}
