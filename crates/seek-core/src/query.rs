//! Query language: hand-written tokenizer and parser producing an [`Expr`] tree.

use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, TimeZone};

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    Term(Term),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Term {
    /// Substring match; `needle` is lowercased.
    Text {
        needle: String,
        on_path: bool,
    },
    /// Anchored glob with `*` / `?`; `pattern` is lowercased.
    Wildcard {
        pattern: String,
        on_path: bool,
    },
    Regex {
        pattern: String,
    },
    /// Lowercased extensions without the dot.
    Ext(Vec<String>),
    /// Inclusive bytes.
    Size {
        min: Option<u64>,
        max: Option<u64>,
    },
    /// Inclusive unix ms.
    Modified {
        min: Option<i64>,
        max: Option<i64>,
    },
    /// Lowercased substring of the drive's display name or of the device (PC) it belongs to.
    Device(String),
    /// Lowercased substring of the drive's display name only; `pc/drive` = that drive on that PC (exact PC name).
    Drive(String),
    /// Lowercased substring of the name of the PC a device belongs to.
    Pc(String),
    FoldersOnly,
    FilesOnly,
    OnlineOnly,
    OfflineOnly,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub message: String,
    /// Char index into the input.
    pub position: usize,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (at {})", self.message, self.position)
    }
}

impl std::error::Error for ParseError {}

pub fn parse(input: &str) -> Result<Expr, ParseError> {
    parse_at(input, chrono::Local::now().fixed_offset())
}

pub fn parse_at(input: &str, now: DateTime<FixedOffset>) -> Result<Expr, ParseError> {
    let mut p = Parser {
        c: input.chars().collect(),
        i: 0,
        now,
    };
    let mut groups = Vec::new();
    loop {
        p.skip_ws();
        if p.i >= p.c.len() {
            break;
        }
        groups.push(p.or_group()?);
    }
    Ok(Expr::And(groups))
}

struct Parser {
    c: Vec<char>,
    i: usize,
    now: DateTime<FixedOffset>,
}

fn err<T>(message: impl Into<String>, position: usize) -> Result<T, ParseError> {
    Err(ParseError {
        message: message.into(),
        position,
    })
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.c.get(self.i).copied()
    }

    fn skip_ws(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.i += 1;
        }
    }

    fn or_group(&mut self) -> Result<Expr, ParseError> {
        let mut items = vec![self.term()?];
        loop {
            let save = self.i;
            self.skip_ws();
            if self.peek() == Some('|') {
                let pipe = self.i;
                self.i += 1;
                self.skip_ws();
                if self.i >= self.c.len() {
                    return err("dangling '|'", pipe);
                }
                items.push(self.term()?);
            } else {
                self.i = save;
                break;
            }
        }
        Ok(if items.len() == 1 {
            items.remove(0)
        } else {
            Expr::Or(items)
        })
    }

    fn term(&mut self) -> Result<Expr, ParseError> {
        let mut negated = false;
        loop {
            if self.peek() != Some('!') {
                break;
            }
            let bang = self.i;
            self.i += 1;
            match self.peek() {
                None | Some('|') => return err("dangling '!'", bang),
                Some(c) if c.is_whitespace() => return err("dangling '!'", bang),
                _ => negated = !negated,
            }
        }
        if self.peek() == Some('|') {
            return err("unexpected '|'", self.i);
        }
        let e = self.token()?;
        Ok(if negated { Expr::Not(Box::new(e)) } else { e })
    }

    /// Read a word up to whitespace (and `|` unless `pipe_ok`), honouring quotes.
    /// Returns the text and whether any unquoted `*`/`?` was present.
    fn word(&mut self, pipe_ok: bool) -> Result<(String, bool), ParseError> {
        let mut s = String::new();
        let mut wild = false;
        while let Some(ch) = self.peek() {
            if ch.is_whitespace() || (ch == '|' && !pipe_ok) {
                break;
            }
            if ch == '"' {
                let open = self.i;
                self.i += 1;
                loop {
                    match self.peek() {
                        None => return err("unclosed quote", open),
                        Some('"') => {
                            self.i += 1;
                            break;
                        }
                        Some(q) => {
                            s.push(q);
                            self.i += 1;
                        }
                    }
                }
                continue;
            }
            if ch == '*' || ch == '?' {
                wild = true;
            }
            s.push(ch);
            self.i += 1;
        }
        Ok((s, wild))
    }

    fn text_term(&self, text: String, wild: bool, on_path: bool) -> Term {
        let mut lower = text.to_lowercase();
        if on_path {
            lower = lower.replace('/', "\\");
        }
        if wild {
            Term::Wildcard {
                pattern: lower,
                on_path,
            }
        } else {
            Term::Text {
                needle: lower,
                on_path,
            }
        }
    }

    fn token(&mut self) -> Result<Expr, ParseError> {
        let start = self.i;
        // Recognise `prefix:`.
        let mut j = self.i;
        while self.c.get(j).is_some_and(|c| c.is_ascii_alphabetic()) {
            j += 1;
        }
        if j > self.i && self.c.get(j) == Some(&':') {
            let key: String = self.c[self.i..j]
                .iter()
                .collect::<String>()
                .to_ascii_lowercase();
            if matches!(
                key.as_str(),
                "ext"
                    | "size"
                    | "dm"
                    | "path"
                    | "regex"
                    | "device"
                    | "drive"
                    | "pc"
                    | "folder"
                    | "file"
                    | "online"
                    | "offline"
            ) {
                self.i = j + 1;
                return self.prefixed(&key, start);
            }
        }
        let (text, wild) = self.word(false)?;
        Ok(Expr::Term(self.text_term(text, wild, false)))
    }

    fn prefixed(&mut self, key: &str, start: usize) -> Result<Expr, ParseError> {
        // Regexes may contain `|`, so they run to the next whitespace.
        let (value, wild) = self.word(key == "regex")?;
        let t = |t: Term| Ok(Expr::Term(t));
        match key {
            "ext" => {
                let exts: Vec<String> = value
                    .split(';')
                    .map(|e| e.trim().trim_start_matches('.').to_lowercase())
                    .filter(|e| !e.is_empty())
                    .collect();
                if exts.is_empty() {
                    return err("ext: needs at least one extension", start);
                }
                t(Term::Ext(exts))
            }
            "size" => {
                let (min, max) = parse_size(&value).map_err(|m| ParseError {
                    message: m,
                    position: start,
                })?;
                t(Term::Size { min, max })
            }
            "dm" => {
                let (min, max) = parse_date_filter(&value, self.now).map_err(|m| ParseError {
                    message: m,
                    position: start,
                })?;
                t(Term::Modified { min, max })
            }
            "regex" => {
                if value.is_empty() {
                    return err("regex: needs a pattern", start);
                }
                if let Err(e) = regex::RegexBuilder::new(&value)
                    .case_insensitive(true)
                    .build()
                {
                    return err(format!("invalid regex: {e}"), start);
                }
                t(Term::Regex { pattern: value })
            }
            "device" => {
                if value.is_empty() {
                    return err("device: needs a name", start);
                }
                t(Term::Device(value.to_lowercase()))
            }
            "drive" => {
                if value.is_empty() {
                    return err("drive: needs a name", start);
                }
                t(Term::Drive(value.to_lowercase()))
            }
            "pc" => {
                if value.is_empty() {
                    return err("pc: needs a name", start);
                }
                t(Term::Pc(value.to_lowercase()))
            }
            "path" => {
                if !value.is_empty() {
                    return t(self.text_term(value, wild, true));
                }
                // `path:` followed by the text it applies to.
                self.skip_ws();
                if self.i >= self.c.len() || self.peek() == Some('|') {
                    return err("path: needs text to match", start);
                }
                let (text, wild) = self.word(false)?;
                t(self.text_term(text, wild, true))
            }
            "folder" | "file" | "online" | "offline" => {
                let flag = match key {
                    "folder" => Term::FoldersOnly,
                    "file" => Term::FilesOnly,
                    "online" => Term::OnlineOnly,
                    _ => Term::OfflineOnly,
                };
                if value.is_empty() {
                    t(flag)
                } else {
                    Ok(Expr::And(vec![
                        Expr::Term(flag),
                        Expr::Term(self.text_term(value, wild, false)),
                    ]))
                }
            }
            _ => unreachable!("prefix list is closed"),
        }
    }
}

fn parse_size_number(s: &str) -> Result<u64, String> {
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    if num.is_empty() {
        return Err(format!("bad size number '{s}'"));
    }
    let n: f64 = num.parse().map_err(|_| format!("bad size number '{s}'"))?;
    let mult: f64 = match unit.to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "kb" => 1024.0,
        "mb" => 1024.0 * 1024.0,
        "gb" => 1024.0 * 1024.0 * 1024.0,
        "tb" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        other => return Err(format!("unknown size unit '{other}'")),
    };
    let v = (n * mult).round();
    if !v.is_finite() || v >= 1.8e19 {
        return Err("size too large".into());
    }
    Ok(v as u64)
}

fn parse_size(v: &str) -> Result<(Option<u64>, Option<u64>), String> {
    if v.is_empty() {
        return Err("size: needs a value".into());
    }
    if let Some(pos) = v.find("..") {
        let (a, b) = (&v[..pos], &v[pos + 2..]);
        if a.is_empty() && b.is_empty() {
            return Err("empty size range".into());
        }
        let min = if a.is_empty() {
            None
        } else {
            Some(parse_size_number(a)?)
        };
        let max = if b.is_empty() {
            None
        } else {
            Some(parse_size_number(b)?)
        };
        return Ok((min, max));
    }
    if let Some(r) = v.strip_prefix(">=") {
        return Ok((Some(parse_size_number(r)?), None));
    }
    if let Some(r) = v.strip_prefix('>') {
        // saturating: nothing is larger than u64::MAX, expressed as an empty range
        return Ok(match parse_size_number(r)?.checked_add(1) {
            Some(m) => (Some(m), None),
            None => (Some(u64::MAX), Some(0)),
        });
    }
    if let Some(r) = v.strip_prefix("<=") {
        return Ok((None, Some(parse_size_number(r)?)));
    }
    if let Some(r) = v.strip_prefix('<') {
        return Ok(match parse_size_number(r)?.checked_sub(1) {
            Some(m) => (None, Some(m)),
            None => (Some(1), Some(0)), // "< 0" matches nothing
        });
    }
    let n = parse_size_number(v)?;
    Ok((Some(n), Some(n)))
}

/// `[start, end]` in unix ms of a `YYYY`, `YYYY-MM`, `YYYY-MM-DD`, `today` or `yesterday` spec.
fn date_interval(spec: &str, now: DateTime<FixedOffset>) -> Result<(i64, i64), String> {
    let offset = *now.offset();
    let day_range = |d: NaiveDate| -> Result<(NaiveDate, NaiveDate), String> {
        let next = d.succ_opt().ok_or("date out of range")?;
        Ok((d, next))
    };
    let (from, to) = match spec.to_ascii_lowercase().as_str() {
        "today" => day_range(now.date_naive())?,
        "yesterday" => day_range(now.date_naive().pred_opt().ok_or("date out of range")?)?,
        _ => {
            let parts: Vec<&str> = spec.split('-').collect();
            let num = |s: &str, max_len: usize| -> Result<u32, String> {
                if s.is_empty() || s.len() > max_len || !s.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(format!("bad date '{spec}'"));
                }
                s.parse().map_err(|_| format!("bad date '{spec}'"))
            };
            let bad = || format!("bad date '{spec}'");
            match parts.as_slice() {
                [y] => {
                    if y.len() != 4 {
                        return Err(bad());
                    }
                    let y = num(y, 4)? as i32;
                    (
                        NaiveDate::from_ymd_opt(y, 1, 1).ok_or_else(bad)?,
                        NaiveDate::from_ymd_opt(y + 1, 1, 1).ok_or_else(bad)?,
                    )
                }
                [y, m] => {
                    if y.len() != 4 {
                        return Err(bad());
                    }
                    let (y, m) = (num(y, 4)? as i32, num(m, 2)?);
                    let from = NaiveDate::from_ymd_opt(y, m, 1).ok_or_else(bad)?;
                    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
                    (from, NaiveDate::from_ymd_opt(ny, nm, 1).ok_or_else(bad)?)
                }
                [y, m, d] => {
                    if y.len() != 4 {
                        return Err(bad());
                    }
                    let from = NaiveDate::from_ymd_opt(num(y, 4)? as i32, num(m, 2)?, num(d, 2)?)
                        .ok_or_else(bad)?;
                    day_range(from)?
                }
                _ => return Err(bad()),
            }
        }
    };
    let ms = |d: NaiveDate| -> Result<i64, String> {
        let naive = d.and_hms_opt(0, 0, 0).ok_or("date out of range")?;
        offset
            .from_local_datetime(&naive)
            .single()
            .map(|dt| dt.timestamp_millis())
            .ok_or_else(|| "date out of range".to_string())
    };
    debug_assert!(from.year() <= to.year());
    Ok((ms(from)?, ms(to)? - 1))
}

fn parse_date_filter(
    v: &str,
    now: DateTime<FixedOffset>,
) -> Result<(Option<i64>, Option<i64>), String> {
    if v.is_empty() {
        return Err("dm: needs a value".into());
    }
    if let Some(pos) = v.find("..") {
        let (a, b) = (&v[..pos], &v[pos + 2..]);
        if a.is_empty() && b.is_empty() {
            return Err("empty date range".into());
        }
        let min = if a.is_empty() {
            None
        } else {
            Some(date_interval(a, now)?.0)
        };
        let max = if b.is_empty() {
            None
        } else {
            Some(date_interval(b, now)?.1)
        };
        return Ok((min, max));
    }
    if let Some(r) = v.strip_prefix(">=") {
        return Ok((Some(date_interval(r, now)?.0), None));
    }
    if let Some(r) = v.strip_prefix('>') {
        return Ok((Some(date_interval(r, now)?.1.saturating_add(1)), None));
    }
    if let Some(r) = v.strip_prefix("<=") {
        return Ok((None, Some(date_interval(r, now)?.1)));
    }
    if let Some(r) = v.strip_prefix('<') {
        return Ok((None, Some(date_interval(r, now)?.0.saturating_sub(1))));
    }
    let (a, b) = date_interval(v, now)?;
    Ok((Some(a), Some(b)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339("2026-10-04T15:00:00+03:00").unwrap()
    }
    fn p(s: &str) -> Expr {
        parse_at(s, now()).unwrap()
    }
    fn t(s: &str) -> Expr {
        Expr::Term(Term::Text {
            needle: s.into(),
            on_path: false,
        })
    }
    fn and1(t: Term) -> Expr {
        Expr::And(vec![Expr::Term(t)])
    }

    #[test]
    fn empty_matches_all() {
        assert_eq!(p(""), Expr::And(vec![]));
        assert_eq!(p("   "), Expr::And(vec![]));
    }

    #[test]
    fn and_or_not_precedence() {
        assert_eq!(
            p("a b | c"),
            Expr::And(vec![t("a"), Expr::Or(vec![t("b"), t("c")])])
        );
        assert_eq!(p("a|b"), Expr::And(vec![Expr::Or(vec![t("a"), t("b")])]));
        assert_eq!(p("!tmp"), Expr::And(vec![Expr::Not(Box::new(t("tmp")))]));
        assert_eq!(p("Foo"), Expr::And(vec![t("foo")]));
    }

    #[test]
    fn quoted() {
        assert_eq!(p("\"my file\""), Expr::And(vec![t("my file")]));
    }

    #[test]
    fn wildcard() {
        assert!(
            matches!(&p("*.PDF"), Expr::And(v) if v[0] == Expr::Term(Term::Wildcard{pattern:"*.pdf".into(), on_path:false}))
        );
    }

    #[test]
    fn ext() {
        assert_eq!(
            p("ext:PDF;docx"),
            Expr::And(vec![Expr::Term(Term::Ext(vec![
                "pdf".into(),
                "docx".into()
            ]))])
        );
    }

    #[test]
    fn size() {
        assert_eq!(
            p("size:>1mb"),
            and1(Term::Size {
                min: Some(1024 * 1024 + 1),
                max: None
            })
        );
        assert_eq!(
            p("size:1kb..2kb"),
            and1(Term::Size {
                min: Some(1024),
                max: Some(2048)
            })
        );
        assert_eq!(
            p("size:<=1.5kb"),
            and1(Term::Size {
                min: None,
                max: Some(1536)
            })
        );
    }

    #[test]
    fn dates() {
        let Expr::And(v) = p("dm:2024") else { panic!() };
        let Expr::Term(Term::Modified { min, max }) = &v[0] else {
            panic!()
        };
        assert_eq!(*min, Some(1704056400000));
        assert_eq!(*max, Some(1735678799999));
        assert!(matches!(&p("dm:today"), Expr::And(_)));
        let Expr::And(v) = p("dm:>=2024-02") else {
            panic!()
        };
        assert!(matches!(
            &v[0],
            Expr::Term(Term::Modified {
                min: Some(_),
                max: None
            })
        ));
    }

    #[test]
    fn path_regex_flags() {
        assert_eq!(
            p("path:uni"),
            and1(Term::Text {
                needle: "uni".into(),
                on_path: true
            })
        );
        assert_eq!(
            p("path: uni"),
            and1(Term::Text {
                needle: "uni".into(),
                on_path: true
            })
        );
        assert!(
            matches!(&p("regex:^a.*z$"), Expr::And(v) if matches!(&v[0], Expr::Term(Term::Regex{..})))
        );
        assert_eq!(p("folder:"), and1(Term::FoldersOnly));
        assert_eq!(p("offline:"), and1(Term::OfflineOnly));
        assert_eq!(p("xyz:foo"), Expr::And(vec![t("xyz:foo")]));
    }

    #[test]
    fn errors_have_positions() {
        for (q, pos) in [
            ("ext:", 0),
            ("a size:>", 2),
            ("\"unclosed", 0),
            ("a |", 2),
            ("!", 0),
            ("regex:(", 0),
            ("dm:2024-13", 0),
        ] {
            let e = parse_at(q, now()).unwrap_err();
            assert_eq!(e.position, pos, "{q}: {}", e.message);
        }
    }

    #[test]
    fn never_panics() {
        let alphabet = [
            'a', '|', '!', '"', ':', '*', '?', ' ', '.', '>', '<', '=', '1', 'm', 'b', 'e', 'x',
            't', 's', 'i', 'z', 'd', '-', 'é',
        ];
        let mut seed = 42u64;
        for _ in 0..20000 {
            let len = (seed % 12) as usize;
            let s: String = (0..len)
                .map(|_| {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    alphabet[(seed >> 33) as usize % alphabet.len()]
                })
                .collect();
            let _ = parse_at(&s, now());
        }
    }

    #[test]
    fn pc_term() {
        let e = parse("pc:laptop report").unwrap();
        match e {
            Expr::And(v) => {
                assert_eq!(v[0], Expr::Term(Term::Pc("laptop".into())));
                assert!(matches!(v[1], Expr::Term(Term::Text { .. })));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(parse("pc:").unwrap_err().message, "pc: needs a name");
    }

}
