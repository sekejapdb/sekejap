//! The statements a PostgreSQL client writes that have no collection in
//! them: the FROM-less `SELECT` of session facts, the `SHOW` family, and the
//! client GUCs a driver `SET`s on connect.
//!
//! Each is recognised HERE, in the parser, and compiles to fixed rows or a
//! projection of the `db_*` catalog rows (`catalog.rs`). Nothing in this
//! file reaches the store.

use super::*;

/// The client GUCs a driver sends on connect, with the value this engine
/// reports for each.
///
/// The set is CLOSED and the values are constants: there is no session
/// object to keep a knob in (a connection is a process here), so a `SET` of
/// one of these is accepted as a NOTICE naming the knob and the value, and a
/// `SHOW` of one answers with the constant below. A driver that sets
/// `client_encoding` and then reads it back gets `UTF8` either way, which is
/// true -- e4 stores text as UTF-8 and has no other encoding.
pub(crate) const CLIENT_GUCS: &[(&str, &str)] = &[
    ("client_encoding", "UTF8"),
    ("server_encoding", "UTF8"),
    ("server_version", "16.0"),
    ("datestyle", "ISO, MDY"),
    ("intervalstyle", "postgres"),
    ("timezone", "UTC"),
    ("application_name", ""),
    ("search_path", "\"$user\", public"),
    ("extra_float_digits", "1"),
    ("standard_conforming_strings", "on"),
    ("integer_datetimes", "on"),
    ("transaction_isolation", "read committed"),
    ("default_transaction_isolation", "read committed"),
    ("transaction_read_only", "off"),
    ("is_superuser", "on"),
    ("max_identifier_length", "63"),
    ("bytea_output", "hex"),
    ("session_authorization", super::super::catalog::USER),
];

/// The value a client GUC reports, if it is one.
pub(crate) fn guc(name: &str) -> Option<&'static str> {
    entry(name).map(|(_, value)| value)
}

/// The CANONICAL name of a client GUC, if it is one. PostgreSQL spells
/// several of its settings more than one way (`TIME ZONE` is `timezone`,
/// `TRANSACTION ISOLATION LEVEL` is `transaction_isolation`), and an answer
/// names the setting rather than the spelling the statement used.
pub(crate) fn guc_name(name: &str) -> Option<&'static str> {
    entry(name).map(|(name, _)| name)
}

fn entry(name: &str) -> Option<(&'static str, &'static str)> {
    let wanted = name.to_ascii_lowercase().replace([' ', '_'], "");
    CLIENT_GUCS
        .iter()
        .find(|(known, _)| known.replace('_', "") == wanted)
        .copied()
}

impl Parser {
    /// A `SELECT` with no `FROM`, if that is what this is.
    ///
    /// Tried before the ordinary SELECT and backed out of if the statement
    /// turns out to have a `FROM`, because the two share their first token
    /// and the select-list grammars are different: `version()` is not a
    /// column expression and must not be parsed as one.
    pub(super) fn session_select(&mut self) -> SqlResult2<Option<Vec<(SessionItem, Option<String>)>>> {
        let mark = self.mark();
        match self.session_items() {
            Ok(Some(items)) if matches!(self.peek(), Tok::Eof | Tok::Semicolon) => Ok(Some(items)),
            _ => {
                self.reset(mark);
                Ok(None)
            }
        }
    }

    fn session_items(&mut self) -> SqlResult2<Option<Vec<(SessionItem, Option<String>)>>> {
        self.expect_word("SELECT")?;
        let mut items = Vec::new();
        loop {
            let Some(item) = self.session_item()? else {
                return Ok(None);
            };
            let alias = if self.eat_word("AS") {
                Some(self.name()?)
            } else if matches!(self.peek(), Tok::Word(_) | Tok::Quoted(_))
                && self.word().as_deref() != Some("FROM")
            {
                Some(self.name()?)
            } else {
                None
            };
            items.push((item, alias));
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        Ok(Some(items))
    }

    fn session_item(&mut self) -> SqlResult2<Option<SessionItem>> {
        if let Some(word) = self.word() {
            let item = match word.as_str() {
                "VERSION" => Some(SessionItem::Version),
                "DB_VERSION" => Some(SessionItem::DbVersion),
                "CURRENT_SCHEMA" => Some(SessionItem::CurrentSchema),
                "CURRENT_DATABASE" => Some(SessionItem::CurrentDatabase),
                "CURRENT_USER" | "SESSION_USER" | "CURRENT_ROLE" | "USER" => {
                    Some(SessionItem::CurrentUser)
                }
                "PG_BACKEND_PID" => Some(SessionItem::BackendPid),
                _ => None,
            };
            if let Some(item) = item {
                self.bump();
                // `current_user` and friends are spelled without parentheses
                // in the standard and with them by some drivers; both are
                // the same fact.
                if self.eat(&Tok::LParen) {
                    self.expect(&Tok::RParen)?;
                }
                return Ok(Some(item));
            }
            if word == "CURRENT_SETTING" {
                self.bump();
                self.expect(&Tok::LParen)?;
                let name = match self.bump() {
                    Tok::Str(text) => text,
                    other => {
                        return Err(SqlError::syntax(
                            format!(
                                "current_setting takes the knob's name as a string, found `{}`",
                                other.written()
                            ),
                            self.here(),
                        ))
                    }
                };
                // The optional second argument is `missing_ok`; either way
                // an unknown knob answers NULL here.
                if self.eat(&Tok::Comma) {
                    let _ = self.bump();
                }
                self.expect(&Tok::RParen)?;
                return Ok(Some(SessionItem::Setting(name)));
            }
        }
        // `SELECT 1`, the liveness probe every pool sends.
        match self.peek().clone() {
            Tok::Num(value, exact) => {
                self.bump();
                Ok(Some(SessionItem::Lit(Literal::Num(value, exact))))
            }
            Tok::Str(text) => {
                self.bump();
                Ok(Some(SessionItem::Lit(Literal::Str(text))))
            }
            _ => Ok(None),
        }
    }

    /// The `SHOW` family of `docs/lang/QL_CONTRACT.md` §2, plus `SHOW <guc>`.
    ///
    /// `SHOW <name>` is ambiguous by construction -- a collection and a
    /// client GUC are both a bare word after `SHOW` -- so the parser keeps
    /// the word and the COMPILER decides against the catalog, which is the
    /// only place the answer is known.
    pub(super) fn show(&mut self) -> SqlResult2<Stmt> {
        self.expect_word("SHOW")?;
        if self.eat_word("TABLES") {
            return Ok(Stmt::Show(Show::Tables));
        }
        if self.eat_word("EDGES") {
            // `SHOW EDGES FROM t [TO t]` is e3's spelling; the filters are a
            // WHERE over `db_edges` here, so the bare form is the statement
            // and the filtered form says where it moved to.
            if matches!(self.word().as_deref(), Some("FROM") | Some("TO")) {
                return Err(SqlError::unsupported(
                    "SHOW EDGES takes no FROM/TO here: it is sugar for `SELECT * FROM db_edges`, so the filter is written as one -- `SELECT * FROM db_edges WHERE from_table = 'x'`",
                ));
            }
            return Ok(Stmt::Show(Show::Edges));
        }
        if self.eat_word("INDEXES") || self.eat_word("INDEX") {
            let table = if self.eat_word("ON") {
                Some(self.name()?)
            } else {
                None
            };
            return Ok(Stmt::Show(Show::Indexes(table)));
        }
        if self.eat_word("CREATE") {
            self.expect_word("TABLE")?;
            return Ok(Stmt::Show(Show::CreateTable(self.name()?)));
        }
        if self.eat_word("ALL") {
            return Err(SqlError::unsupported(
                "SHOW ALL lists a settings table; e4 has none. The client GUCs a driver sets are accepted as notices and each answers `SHOW <name>` from a constant",
            ));
        }
        // A collection, or a client GUC. PostgreSQL spells several of its
        // settings as WORDS rather than as the knob's name -- `SHOW TIME
        // ZONE` is `timezone`, `SHOW TRANSACTION ISOLATION LEVEL` is
        // `transaction_isolation` -- so the words are collected and the
        // LONGEST prefix that names a known setting wins. A single word that
        // names no setting is a collection, which is the common case.
        let mut words = Vec::new();
        while matches!(self.peek(), Tok::Word(_) | Tok::Quoted(_)) && words.len() < 4 {
            match self.bump() {
                Tok::Word(word) | Tok::Quoted(word) => words.push(word),
                _ => unreachable!("the token was just matched"),
            }
        }
        if words.is_empty() {
            return Err(SqlError::syntax(
                format!(
                    "SHOW takes TABLES, EDGES, INDEXES [ON t], CREATE TABLE t, a collection or a setting name, found `{}`",
                    self.peek().written()
                ),
                self.here(),
            ));
        }
        for take in (1..=words.len()).rev() {
            if let Some(canonical) = guc_name(&words[..take].join("_")) {
                return Ok(Stmt::Show(Show::Name(canonical.to_owned())));
            }
        }
        if words.len() > 1 {
            return Err(SqlError::syntax(
                format!(
                    "SHOW {}: `{}` is not a setting this engine has, and a collection is named by ONE word",
                    words.join(" "),
                    words.join("_")
                ),
                self.here(),
            ));
        }
        Ok(Stmt::Show(Show::Name(words.remove(0))))
    }

    /// `SET [LOCAL|SESSION] <name> = <value>`, `SET TIME ZONE ...` and
    /// `RESET <name>`.
    ///
    /// Two destinations. The two knobs this engine HAS (`ef_search` and
    /// `diskann.query_search_list_size`) become [`Stmt::SetLocal`] and change
    /// the session's approximate shortlist bound. Everything else becomes
    /// [`Stmt::SetGuc`]: a notice naming the knob and the value, because
    /// there is nothing to set and a silent `SET` reads as one that took
    /// effect.
    pub(super) fn set_local(&mut self) -> SqlResult2<Stmt> {
        let reset = self.word().as_deref() == Some("RESET");
        if reset {
            self.bump();
        } else {
            self.expect_word("SET")?;
        }
        let _ = self.eat_word("LOCAL") || self.eat_word("SESSION");
        // `SET TIME ZONE 'UTC'` and `SET SESSION CHARACTERISTICS AS
        // TRANSACTION ...` are spelled with words rather than a knob name.
        if self.word().as_deref() == Some("TIME") {
            self.bump();
            self.expect_word("ZONE")?;
            let value = self.guc_value();
            return Ok(Stmt::SetGuc {
                name: "TimeZone".into(),
                value,
            });
        }
        if self.word().as_deref() == Some("CHARACTERISTICS") {
            let value = self.guc_value();
            return Ok(Stmt::SetGuc {
                name: "session characteristics".into(),
                value,
            });
        }
        if reset && self.word().as_deref() == Some("ALL") {
            self.bump();
            return Ok(Stmt::SetGuc {
                name: "all".into(),
                value: "DEFAULT".into(),
            });
        }
        let mut name = self.name()?;
        // `diskann.query_search_list_size` arrives as two dotted names, and
        // `name()` keeps only the last segment; the knob is the whole thing.
        if name.eq_ignore_ascii_case("query_search_list_size")
            || name.eq_ignore_ascii_case("query_rescore")
        {
            name = format!("diskann.{name}");
        }
        if reset {
            return Ok(Stmt::SetGuc {
                name,
                value: "DEFAULT".into(),
            });
        }
        if !self.eat(&Tok::Eq) {
            self.expect_word("TO")?;
        }
        let engine_knob = matches!(
            name.to_ascii_lowercase().as_str(),
            "ef_search" | "hnsw.ef_search" | "diskann.query_search_list_size"
        );
        if engine_knob {
            let value = if let Some(word) = self.word() {
                match word.as_str() {
                    "ON" | "OFF" | "DEFAULT" => {
                        self.bump();
                        Literal::Str(word.to_ascii_lowercase())
                    }
                    _ => self.literal()?,
                }
            } else {
                self.literal()?
            };
            return Ok(Stmt::SetLocal { name, value });
        }
        let value = self.guc_value();
        Ok(Stmt::SetGuc { name, value })
    }

    /// Everything from the cursor to the end of the statement, as the text a
    /// notice quotes back.
    ///
    /// A client GUC's value is not a value this engine reads: `search_path`
    /// is a comma list, `DateStyle` is two words, `TimeZone` is a string or
    /// the word `LOCAL`. Nothing here parses them, so nothing here has to
    /// know their grammars -- the tokens are written back the way they were
    /// spelled.
    fn guc_value(&mut self) -> String {
        let mut parts: Vec<String> = Vec::new();
        while !matches!(self.peek(), Tok::Eof | Tok::Semicolon) {
            parts.push(self.bump().written());
        }
        parts.join(" ")
    }
}
