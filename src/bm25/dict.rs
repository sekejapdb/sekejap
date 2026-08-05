//! Term dictionary for BM25.
//!
//! Maps each term to the location of its postings list in the postings file.

use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct TermEntry {
    pub postings_offset: u64,
    pub postings_len: u32,
}

pub struct TermDict {
    entries: HashMap<String, TermEntry>,
}

impl TermDict {
    pub fn new() -> Self {
        Self { entries: HashMap::new() }
    }

    pub fn insert(&mut self, term: String, postings_offset: u64, postings_len: u32) {
        self.entries.insert(term, TermEntry { postings_offset, postings_len });
    }

    /// Approximate resident RAM: term-string bytes + entry structs + map overhead.
    pub fn mem_bytes(&self) -> usize {
        let e = std::mem::size_of::<TermEntry>();
        self.entries.capacity() * (std::mem::size_of::<String>() + e + 8)
            + self.entries.keys().map(|k| k.capacity()).sum::<usize>()
    }

    pub fn get(&self, term: &str) -> Option<&TermEntry> {
        self.entries.get(term)
    }

    pub fn num_terms(&self) -> usize {
        self.entries.len()
    }
}

impl Default for TermDict {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dict_insert_get_roundtrip() {
        let mut dict = TermDict::new();
        dict.insert("rust".to_string(), 0, 100);
        dict.insert("tutorial".to_string(), 100, 200);
        dict.insert("programming".to_string(), 300, 150);

        assert_eq!(dict.get("rust").unwrap().postings_len, 100);
        assert_eq!(dict.get("tutorial").unwrap().postings_offset, 100);
        assert_eq!(dict.num_terms(), 3);
        assert!(dict.get("absent").is_none());
    }
}
