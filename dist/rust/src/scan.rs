//! The lazy walk of one collection. `docs/dist/RUST_API.md` §2.

use crate::db::{with_key, Db};
use crate::error::Result;
use crate::Document;
use sekejap_core::collections::{CollectionId, EntityId};
use std::collections::VecDeque;

/// One collection in stable id order, a page of rows at a time.
///
/// Law 1 at the API: the walk holds at most `page_size` rows and takes the
/// read lock once per page, never once for the whole collection.
pub struct Scan<'a> {
    db: &'a Db,
    collection: String,
    id: CollectionId,
    page_size: usize,
    buffered: VecDeque<Document>,
    after: Option<EntityId>,
    done: bool,
}

impl<'a> Scan<'a> {
    pub(crate) fn new(db: &'a Db, collection: String, id: CollectionId, page_size: usize) -> Self {
        Self {
            db,
            collection,
            id,
            page_size: page_size.max(1),
            buffered: VecDeque::new(),
            after: None,
            done: false,
        }
    }

    /// How many rows one page holds. The default is
    /// [`crate::db::SCAN_PAGE`].
    pub fn page_size(mut self, rows: usize) -> Self {
        self.page_size = rows.max(1);
        self
    }

    /// The collection being walked.
    pub fn collection(&self) -> &str {
        &self.collection
    }

    fn fill(&mut self) -> Result<()> {
        let (id, after, page_size, name) =
            (self.id, self.after, self.page_size, self.collection.clone());
        let page = self.db.read(|db| {
            let mut out = Vec::with_capacity(page_size);
            for row in db.scan(id, after)?.take(page_size) {
                let entity = row?;
                out.push(Document {
                    collection: name.clone(),
                    key: entity.key.clone(),
                    id: entity.id,
                    fields: with_key(entity.document, &entity.key),
                });
            }
            Ok(out)
        })?;
        match page.last() {
            None => self.done = true,
            Some(last) => self.after = Some(last.id),
        }
        self.buffered.extend(page);
        Ok(())
    }
}

impl Iterator for Scan<'_> {
    type Item = Result<Document>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(doc) = self.buffered.pop_front() {
            return Some(Ok(doc));
        }
        if self.done {
            return None;
        }
        if let Err(e) = self.fill() {
            self.done = true;
            return Some(Err(e));
        }
        self.buffered.pop_front().map(Ok)
    }
}
