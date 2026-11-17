//! Keyset pagination over ids. Ids are UUID v7, so "after this id" is "created after this row".

use serde::Serialize;
use uuid::Uuid;

use crate::error::AppError;

const DEFAULT_LIMIT: u32 = 50;
const MAX_LIMIT: u32 = 200;

#[derive(Debug, Serialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// Pass as `cursor` to get the next page; `null` on the last page.
    pub next_cursor: Option<Uuid>,
}

/// The page size to use for the `limit` parameter.
pub fn page_size(limit: Option<u32>) -> Result<u32, AppError> {
    match limit {
        None => Ok(DEFAULT_LIMIT),
        Some(n) if (1..=MAX_LIMIT).contains(&n) => Ok(n),
        Some(_) => Err(AppError::invalid("limit", "out_of_range")),
    }
}

/// Builds a page from `size + 1` rows: the extra row only tells whether another page exists.
pub fn page<T>(mut rows: Vec<T>, size: u32, id_of: impl Fn(&T) -> Uuid) -> Page<T> {
    let size = size as usize;
    let has_more = rows.len() > size;
    rows.truncate(size);
    let next_cursor = if has_more {
        rows.last().map(id_of)
    } else {
        None
    };
    Page {
        items: rows,
        next_cursor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_bounds() {
        assert_eq!(page_size(None).unwrap(), 50);
        assert_eq!(page_size(Some(200)).unwrap(), 200);
        assert!(page_size(Some(0)).is_err());
        assert!(page_size(Some(201)).is_err());
    }

    #[test]
    fn extra_row_becomes_the_cursor() {
        let ids: Vec<Uuid> = (1..=3).map(Uuid::from_u128).collect();
        let full = page(ids.clone(), 2, |id| *id);
        assert_eq!(full.items, ids[..2]);
        assert_eq!(full.next_cursor, Some(ids[1]));

        let last = page(ids.clone(), 3, |id| *id);
        assert_eq!(last.items, ids);
        assert_eq!(last.next_cursor, None);
    }
}
