//! The check that keeps `docs/lang/GQL_FEATURES.md` true (M7-A, M7-E of
//! `docs/lang/GQL_PROFILE_DESIGN_M5_M7.md` §4.2 and §4.7). Test-only: it adds
//! no public surface and ships nothing.
//!
//! 1. Every registry row names a test that exists: the file under the
//!    repository and a `fn` of that name in it. A construct without a test is
//!    not Tier 1.
//! 2. Every function of the profile (`Func::ALL`, `GraphFunc::ALL`,
//!    `AggFunc::ALL`, which are crate-private, hence the check lives here)
//!    and every statement keyword the parser accepts by position has a row.
//! 3. The unsupported list equals `refuse::GQL_TABLE`, both ways, construct,
//!    tier and reason: a new refusal cannot ship unpublished, and a published
//!    one cannot outlive its row.

#[cfg(test)]
mod tests {
    use super::super::ast::{AggFunc, Func, GraphFunc};
    use crate::refuse::GQL_TABLE;
    use crate::Tier;
    use std::path::{Path, PathBuf};

    /// Every statement keyword the GQL parser accepts by position. A new one
    /// is added here, and the registry must then name it.
    const KEYWORDS: &[&str] = &[
        "MATCH", "OPTIONAL MATCH", "LET", "FILTER", "FOR", "RETURN", "NEXT", "CALL", "EXISTS", "UNION", "WALK",
        "TRAIL", "ACYCLIC", "ANY", "SHORTEST", "CHEAPEST", "COST",
    ];

    fn root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("the workspace root").to_owned()
    }

    fn document() -> String {
        std::fs::read_to_string(root().join("docs/lang/GQL_FEATURES.md")).expect("docs/lang/GQL_FEATURES.md")
    }

    /// The cells of a table row, split on unescaped `|`, trimmed, with `\|`
    /// read back as `|`.
    fn cells(line: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut cell = String::new();
        let mut chars = line.trim().trim_start_matches('|').chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\\' if chars.peek() == Some(&'|') => {
                    cell.push('|');
                    chars.next();
                }
                '|' => out.push(std::mem::take(&mut cell).trim().to_owned()),
                other => cell.push(other),
            }
        }
        out
    }

    /// The body rows of the table under the heading `## {section}`.
    fn table(document: &str, section: &str) -> Vec<Vec<String>> {
        let heading = format!("## {section}\n");
        let at = document.find(&heading).unwrap_or_else(|| panic!("no section `{section}`"));
        document[at + heading.len()..]
            .lines()
            .skip_while(|line| !line.starts_with('|'))
            .take_while(|line| line.starts_with('|'))
            .skip(2)
            .map(cells)
            .collect()
    }

    /// True when `word` stands in `text` as a whole word.
    fn names(text: &str, word: &str) -> bool {
        text.match_indices(word).any(|(at, _)| {
            let before = text[..at].chars().next_back();
            let after = text[at + word.len()..].chars().next();
            let edge = |c: Option<char>| !c.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
            edge(before) && edge(after)
        })
    }

    #[test]
    fn every_registry_row_names_a_test_that_exists() {
        let rows = table(&document(), "Registry");
        assert!(rows.len() > 30, "the registry has {} rows", rows.len());
        for row in &rows {
            assert_eq!(row.len(), 4, "a registry row has four cells: {row:?}");
            assert_eq!(row[2], "T1", "a registry row is a built construct: {row:?}");
            let test = row[3].trim_matches('`');
            let (file, name) = test.split_once("::").unwrap_or_else(|| panic!("`file::fn`, not `{test}`"));
            let source = std::fs::read_to_string(root().join(file)).unwrap_or_else(|_| panic!("`{file}` does not exist"));
            assert!(source.contains(&format!("fn {name}(")), "`{file}` has no `fn {name}`");
        }
    }

    #[test]
    fn every_function_and_keyword_of_the_profile_has_a_row() {
        let constructs: Vec<String> = table(&document(), "Registry").into_iter().map(|row| row[0].clone()).collect();
        let functions = Func::ALL
            .iter()
            .map(|f| f.written())
            .chain(GraphFunc::ALL.iter().map(|f| f.written()))
            .chain(AggFunc::ALL.iter().map(|f| f.written()));
        for word in functions.chain(KEYWORDS.iter().copied()) {
            assert!(
                constructs.iter().any(|construct| names(construct, word)),
                "`{word}` has no registry row"
            );
        }
    }

    #[test]
    fn the_unsupported_list_is_the_refusal_table() {
        let published: Vec<(String, String, String)> = table(&document(), "Unsupported")
            .into_iter()
            .map(|row| {
                assert_eq!(row.len(), 3, "an unsupported row has three cells: {row:?}");
                (row[0].trim_matches('`').to_owned(), row[1].clone(), row[2].clone())
            })
            .collect();
        let refused: Vec<(String, String, String)> = GQL_TABLE
            .iter()
            .map(|(construct, tier, reason)| {
                let tier = match tier {
                    Tier::Two => "T2",
                    Tier::Three => "T3",
                };
                ((*construct).to_owned(), tier.to_owned(), (*reason).to_owned())
            })
            .collect();
        for row in &refused {
            assert!(published.contains(row), "refused but not published: {row:?}");
        }
        for row in &published {
            assert!(refused.contains(row), "published but not refused: {row:?}");
        }
    }
}
