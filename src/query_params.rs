//! Parameters on saved queries (goodlux, 2026-11-17).
//!
//! A saved query declares what it takes in its frontmatter, under its class's
//! `parameter` property: one value per parameter, written
//! `name[:kind][=default]`. No default means it must be given. The query
//! text uses it as the variable `?name`.
//!
//! The KIND is the author's to state, because the author knows what the
//! query compares the value with and the caller often does not. With no kind
//! a value is text, always: nothing is guessed from how a value looks, so an
//! id of `42` stays the text 42. A stated kind is checked.
//! It is run as `git lex query copia/being-description being=sylkie-glitch`.
//!
//! A parameter is a VALUE. It is put into the parsed query in place of the
//! variable, the way SHACL pre-binds `sh:parameter` values, and the query is
//! printed back by the SPARQL printer. It is never pasted into the text, so
//! nothing a caller writes can change what the query asks.

use spargebra::algebra::{AggregateExpression, Expression, GraphPattern, OrderExpression};
use spargebra::term::{Literal, NamedNode, NamedNodePattern, TermPattern, TriplePattern, Variable};
use spargebra::Query;

/// The last segment of the frontmatter key that declares parameters
/// (`soul.Query.parameter`, `copia.Query.parameter`).
pub const PARAMETER_KEY: &str = "parameter";

/// Where a relative document address (`<soul/Query/recent>`) is rooted.
const ADDRESS_ROOT: &str = crate::PLATFORM_ROOT;

const XSD: &str = "http://www.w3.org/2001/XMLSchema#";

/// One declared parameter.
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: String,
    pub kind: Kind,
    pub default: Option<String>,
}

/// What kind of value a parameter takes. The words are the XML Schema names
/// the ontology already uses for ranges, plus `iri` for a document's address.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    String,
    Integer,
    Decimal,
    Boolean,
    Date,
    DateTime,
    Iri,
}

const KINDS: &[(&str, Kind)] = &[
    ("string", Kind::String),
    ("integer", Kind::Integer),
    ("decimal", Kind::Decimal),
    ("boolean", Kind::Boolean),
    ("date", Kind::Date),
    ("dateTime", Kind::DateTime),
    ("iri", Kind::Iri),
];

impl Kind {
    fn word(self) -> &'static str {
        KINDS.iter().find(|(_, k)| *k == self).map(|(w, _)| *w).unwrap_or("string")
    }
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The parameters a query document's frontmatter declares, in order.
pub fn declared(frontmatter: Option<&str>) -> Result<Vec<Param>, String> {
    let Some(yaml) = frontmatter else { return Ok(Vec::new()) };
    let map = crate::parse_frontmatter_map(yaml)?;
    let mut out: Vec<Param> = Vec::new();
    for (key, value) in &map {
        let Some(key) = key.as_str() else { continue };
        if key.rsplit('.').next() != Some(PARAMETER_KEY) {
            continue;
        }
        let entries: Vec<String> = match value {
            serde_yaml::Value::Null => Vec::new(),
            serde_yaml::Value::String(s) => vec![s.clone()],
            serde_yaml::Value::Sequence(items) => {
                let mut list = Vec::new();
                for item in items {
                    match item {
                        serde_yaml::Value::String(s) => list.push(s.clone()),
                        other => {
                            return Err(format!(
                                "`{key}` holds {other:?}, which is not text. Write each parameter in quotes: \"name\" or \"name=default\"."
                            ));
                        }
                    }
                }
                list
            }
            other => {
                return Err(format!("`{key}` holds {other:?}. Write a list of parameters, each \"name\" or \"name=default\"."));
            }
        };
        for entry in entries {
            let (head, default) = match entry.split_once('=') {
                Some((h, d)) => (h.trim(), Some(d.trim().to_string())),
                None => (entry.trim(), None),
            };
            let (name, kind) = match head.split_once(':') {
                Some((n, k)) => {
                    let k = k.trim();
                    let Some((_, kind)) = KINDS.iter().find(|(word, _)| *word == k) else {
                        let words: Vec<&str> = KINDS.iter().map(|(w, _)| *w).collect();
                        return Err(format!(
                            "`{key}` gives the parameter '{}' the kind '{k}', which is not one. The kinds are: {}.",
                            n.trim(),
                            words.join(", ")
                        ));
                    };
                    (n.trim(), *kind)
                }
                None => (head, Kind::String),
            };
            let name = name.strip_prefix('?').unwrap_or(name).to_string();
            if !valid_name(&name) {
                return Err(format!(
                    "`{key}` declares a parameter named '{name}'. A name is letters, digits and underscores, starting with a letter."
                ));
            }
            if out.iter().any(|p| p.name == name) {
                return Err(format!("`{key}` declares the parameter '{name}' twice."));
            }
            // A default that does not fit its own kind is the author's
            // mistake: say so when the document is read, not at a later run.
            if let Some(d) = &default {
                read_value(&name, kind, d).map_err(|e| format!("`{key}`: the default is wrong. {e}"))?;
            }
            out.push(Param { name, kind, default });
        }
    }
    Ok(out)
}

/// What a caller's text means as a value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Iri(NamedNode),
    Literal(Literal),
}

fn typed(text: &str, datatype: &str) -> Value {
    Value::Literal(Literal::new_typed_literal(text, NamedNode::new_unchecked(format!("{XSD}{datatype}"))))
}

fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn is_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && is_digits(&s[..4])
        && matches!(s[5..7].parse::<u8>(), Ok(1..=12))
        && s.is_char_boundary(8)
        && matches!(s[8..].parse::<u8>(), Ok(1..=31))
        && is_digits(&s[5..7])
        && is_digits(&s[8..])
}

fn is_time(s: &str) -> bool {
    // hh:mm:ss, an optional fraction, an optional Z or +hh:mm / -hh:mm.
    let b = s.as_bytes();
    if b.len() < 8 || b[2] != b':' || b[5] != b':' || !is_digits(&s[..2]) || !is_digits(&s[3..5]) || !is_digits(&s[6..8]) {
        return false;
    }
    let mut rest = &s[8..];
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return false;
        }
        rest = &fraction[digits..];
    }
    match rest {
        "" | "Z" => true,
        zone => {
            let z = zone.as_bytes();
            z.len() == 6 && (z[0] == b'+' || z[0] == b'-') && z[3] == b':' && is_digits(&zone[1..3]) && is_digits(&zone[4..])
        }
    }
}

/// Read a value as the kind its parameter declares. Text is taken exactly as
/// written. Every other kind is checked, and a value that does not fit is an
/// `Err` saying what the parameter takes.
///
/// An address is written with or without `<...>`. One without a scheme
/// (`soul/Query/recent`) is the short form used in frontmatter.
pub fn read_value(name: &str, kind: Kind, raw: &str) -> Result<Value, String> {
    read_value_at(name, kind, raw, ADDRESS_ROOT)
}

/// [`read_value`] with the address a short `iri` value is read against: the
/// root of the store being asked.
pub fn read_value_at(name: &str, kind: Kind, raw: &str, base: &str) -> Result<Value, String> {
    let wrong = |takes: &str| format!("The parameter '{name}' takes {takes}, and was given '{raw}'.");
    let unsigned = raw.strip_prefix('-').unwrap_or(raw);
    match kind {
        Kind::String => Ok(Value::Literal(Literal::new_simple_literal(raw))),
        Kind::Integer if is_digits(unsigned) => Ok(typed(raw, "integer")),
        Kind::Integer => Err(wrong("a whole number")),
        Kind::Decimal => match unsigned.split_once('.') {
            Some((whole, fraction)) if is_digits(whole) && is_digits(fraction) => Ok(typed(raw, "decimal")),
            None if is_digits(unsigned) => Ok(typed(raw, "decimal")),
            _ => Err(wrong("a number, such as 1.5")),
        },
        Kind::Boolean if raw == "true" || raw == "false" => Ok(typed(raw, "boolean")),
        Kind::Boolean => Err(wrong("true or false")),
        Kind::Date if is_date(raw) => Ok(typed(raw, "date")),
        Kind::Date => Err(wrong("a date, such as 2026-11-17")),
        Kind::DateTime if raw.len() > 11 && raw.is_char_boundary(10) && is_date(&raw[..10]) && raw.as_bytes()[10] == b'T' && is_time(&raw[11..]) => {
            Ok(typed(raw, "dateTime"))
        }
        Kind::DateTime => Err(wrong("a date and time, such as 2026-11-17T09:00:00-08:00")),
        Kind::Iri => {
            let inner = raw.strip_prefix('<').and_then(|r| r.strip_suffix('>')).unwrap_or(raw);
            let full = if inner.contains("://") || inner.starts_with("urn:") { inner.to_string() } else { format!("{base}{inner}") };
            if inner.is_empty() {
                return Err(wrong("a document's address"));
            }
            NamedNode::new(&full).map(Value::Iri).map_err(|_| wrong("a document's address"))
        }
    }
}

/// Split the `name=value` words a caller gave.
pub fn read_given(words: &[String]) -> Result<Vec<(String, String)>, String> {
    let mut out: Vec<(String, String)> = Vec::new();
    for word in words {
        let Some((name, value)) = word.split_once('=') else {
            return Err(format!("'{word}' carries no value. A parameter is written name=value."));
        };
        if out.iter().any(|(n, _)| n == name) {
            return Err(format!("The parameter '{name}' is given twice."));
        }
        out.push((name.to_string(), value.to_string()));
    }
    Ok(out)
}

fn describe(declared: &[Param]) -> String {
    if declared.is_empty() {
        return "It takes none.".to_string();
    }
    let list: Vec<String> = declared
        .iter()
        .map(|p| {
            let kind = if p.kind == Kind::String { String::new() } else { format!("{}, ", p.kind.word()) };
            match &p.default {
                Some(d) => format!("{} ({kind}default {d})", p.name),
                None => format!("{} ({kind}must be given)", p.name),
            }
        })
        .collect();
    format!("It takes: {}.", list.join(", "))
}

/// The query with every declared parameter put in as a value.
///
/// A query that declares none and is given none comes back untouched, word
/// for word. Otherwise `Err` says, in order of checking: a name the query
/// does not declare; a parameter with no value and no default; a value that
/// cannot go where the query uses it; a declared parameter the query never
/// uses or sets itself.
pub fn bind(root: Option<&std::path::Path>, text: &str, declared: &[Param], given: &[(String, String)]) -> Result<String, String> {
    if declared.is_empty() && given.is_empty() {
        return Ok(text.to_string());
    }
    if let Some((name, _)) = given.iter().find(|(n, _)| !declared.iter().any(|p| &p.name == n)) {
        return Err(format!("This query has no parameter named '{name}'. {}", describe(declared)));
    }
    // Short addresses, in a value and in the query's own text, are read
    // against the root of the store being asked.
    let base = root.map_or_else(|| crate::PLATFORM_ROOT.to_string(), crate::store_root_at);
    let mut values: Vec<(Variable, Value)> = Vec::new();
    for p in declared {
        let raw = given.iter().find(|(n, _)| n == &p.name).map(|(_, v)| v.as_str()).or(p.default.as_deref());
        let Some(raw) = raw else {
            return Err(format!("The parameter '{0}' is missing and has no default. Give it as {0}=value. {1}", p.name, describe(declared)));
        };
        values.push((Variable::new_unchecked(p.name.clone()), read_value_at(&p.name, p.kind, raw, &base)?));
    }
    let with_prefixes = crate::add_prefixes_at(root, text);
    let mut query = spargebra::SparqlParser::new()
        .with_base_iri(&base)
        .map_err(|e| format!("The store's address {base} is not usable: {e}"))?
        .parse_query(&with_prefixes)
        .map_err(|e| format!("The query does not parse: {e}"))?;
    let mut binder = Binder { values: &values, used: vec![false; values.len()] };
    match &mut query {
        Query::Select { pattern, .. } | Query::Describe { pattern, .. } | Query::Ask { pattern, .. } => binder.pattern(pattern)?,
        Query::Construct { template, pattern, .. } => {
            for triple in template.iter_mut() {
                binder.triple(triple)?;
            }
            binder.pattern(pattern)?;
        }
    }
    if let Some(i) = binder.used.iter().position(|u| !u) {
        return Err(format!(
            "This query declares the parameter '{0}' and never uses it. Use it in the query as ?{0}, or take it out of the declaration.",
            values[i].0.as_str()
        ));
    }
    let bound = query.to_string();
    spargebra::SparqlParser::new()
        .parse_query(&bound)
        .map_err(|e| format!("With its parameters filled in, the query no longer parses ({e})."))?;
    Ok(bound)
}

struct Binder<'a> {
    values: &'a [(Variable, Value)],
    used: Vec<bool>,
}

impl Binder<'_> {
    fn value(&mut self, v: &Variable) -> Option<Value> {
        let i = self.values.iter().position(|(var, _)| var == v)?;
        self.used[i] = true;
        Some(self.values[i].1.clone())
    }

    fn is_param(&self, v: &Variable) -> bool {
        self.values.iter().any(|(var, _)| var == v)
    }

    fn must_be_address(v: &Variable, place: &str) -> String {
        format!("The query uses ?{0} as {place}, so the parameter '{0}' must be declared as an address: {0}:iri.", v.as_str())
    }

    fn sets_it_itself(v: &Variable) -> String {
        format!("The query gives ?{0} a value itself, so '{0}' cannot also be a parameter.", v.as_str())
    }

    fn term(&mut self, t: &mut TermPattern) -> Result<(), String> {
        match t {
            TermPattern::Variable(v) => {
                if let Some(value) = self.value(v) {
                    *t = match value {
                        Value::Iri(n) => TermPattern::NamedNode(n),
                        Value::Literal(l) => TermPattern::Literal(l),
                    };
                }
            }
            TermPattern::Triple(inner) => self.triple(inner)?,
            _ => {}
        }
        Ok(())
    }

    fn named(&mut self, n: &mut NamedNodePattern, place: &str) -> Result<(), String> {
        if let NamedNodePattern::Variable(v) = n {
            match self.value(v) {
                Some(Value::Iri(node)) => *n = NamedNodePattern::NamedNode(node),
                Some(Value::Literal(_)) => return Err(Self::must_be_address(v, place)),
                None => {}
            }
        }
        Ok(())
    }

    fn triple(&mut self, t: &mut TriplePattern) -> Result<(), String> {
        self.term(&mut t.subject)?;
        self.named(&mut t.predicate, "a property")?;
        self.term(&mut t.object)
    }

    fn as_expression(value: Value) -> Expression {
        match value {
            Value::Iri(n) => Expression::NamedNode(n),
            Value::Literal(l) => Expression::Literal(l),
        }
    }

    fn expression(&mut self, e: &mut Expression) -> Result<(), String> {
        match e {
            Expression::NamedNode(_) | Expression::Literal(_) => {}
            Expression::Variable(v) => {
                if let Some(value) = self.value(v) {
                    *e = Self::as_expression(value);
                }
            }
            Expression::Bound(v) => {
                if self.value(v).is_some() {
                    *e = Expression::Literal(Literal::new_typed_literal("true", NamedNode::new_unchecked(format!("{XSD}boolean"))));
                }
            }
            Expression::Or(a, b)
            | Expression::And(a, b)
            | Expression::Equal(a, b)
            | Expression::SameTerm(a, b)
            | Expression::Greater(a, b)
            | Expression::GreaterOrEqual(a, b)
            | Expression::Less(a, b)
            | Expression::LessOrEqual(a, b)
            | Expression::Add(a, b)
            | Expression::Subtract(a, b)
            | Expression::Multiply(a, b)
            | Expression::Divide(a, b) => {
                self.expression(a)?;
                self.expression(b)?;
            }
            Expression::UnaryPlus(a) | Expression::UnaryMinus(a) | Expression::Not(a) => self.expression(a)?,
            Expression::In(a, list) => {
                self.expression(a)?;
                for item in list {
                    self.expression(item)?;
                }
            }
            Expression::Exists(p) => self.pattern(p)?,
            Expression::If(a, b, c) => {
                self.expression(a)?;
                self.expression(b)?;
                self.expression(c)?;
            }
            Expression::Coalesce(list) | Expression::FunctionCall(_, list) => {
                for item in list {
                    self.expression(item)?;
                }
            }
        }
        Ok(())
    }

    fn pattern(&mut self, p: &mut GraphPattern) -> Result<(), String> {
        match p {
            GraphPattern::Bgp { patterns } => {
                for t in patterns {
                    self.triple(t)?;
                }
            }
            GraphPattern::Path { subject, object, .. } => {
                self.term(subject)?;
                self.term(object)?;
            }
            GraphPattern::Join { left, right } | GraphPattern::Union { left, right } | GraphPattern::Minus { left, right } => {
                self.pattern(left)?;
                self.pattern(right)?;
            }
            GraphPattern::LeftJoin { left, right, expression } => {
                self.pattern(left)?;
                self.pattern(right)?;
                if let Some(e) = expression {
                    self.expression(e)?;
                }
            }
            GraphPattern::Filter { expr, inner } => {
                self.expression(expr)?;
                self.pattern(inner)?;
            }
            GraphPattern::Graph { name, inner } => {
                self.named(name, "a graph's name")?;
                self.pattern(inner)?;
            }
            GraphPattern::Service { name, inner, .. } => {
                self.named(name, "a service's address")?;
                self.pattern(inner)?;
            }
            GraphPattern::Extend { inner, variable, expression } => {
                if self.is_param(variable) {
                    return Err(Self::sets_it_itself(variable));
                }
                self.expression(expression)?;
                self.pattern(inner)?;
            }
            GraphPattern::Values { variables, .. } => {
                if let Some(v) = variables.iter().find(|v| self.is_param(v)) {
                    return Err(Self::sets_it_itself(v));
                }
            }
            GraphPattern::OrderBy { inner, expression } => {
                for order in expression {
                    match order {
                        OrderExpression::Asc(e) | OrderExpression::Desc(e) => self.expression(e)?,
                    }
                }
                self.pattern(inner)?;
            }
            GraphPattern::Project { inner, variables } => {
                self.pattern(inner)?;
                // A parameter the query also returns keeps its column: the
                // value is put back under the name after the pattern ran.
                for v in variables.iter() {
                    if let Some(value) = self.value(v) {
                        let taken = std::mem::replace(inner.as_mut(), GraphPattern::Bgp { patterns: Vec::new() });
                        **inner = GraphPattern::Extend { inner: Box::new(taken), variable: v.clone(), expression: Self::as_expression(value) };
                    }
                }
            }
            GraphPattern::Distinct { inner } | GraphPattern::Reduced { inner } | GraphPattern::Slice { inner, .. } => self.pattern(inner)?,
            GraphPattern::Group { inner, variables, aggregates } => {
                for (target, aggregate) in aggregates.iter_mut() {
                    if self.is_param(target) {
                        return Err(Self::sets_it_itself(target));
                    }
                    if let AggregateExpression::FunctionCall { expr, .. } = aggregate {
                        self.expression(expr)?;
                    }
                }
                // Grouping by a fixed value groups by nothing.
                variables.retain(|v| !self.values.iter().any(|(var, _)| var == v));
                self.pattern(inner)?;
            }
            // A form of pattern this code does not know (the SPARQL library
            // grows them behind features): refuse, never skip it and leave a
            // parameter unfilled inside.
            #[allow(unreachable_patterns)]
            _ => return Err("The query uses a form of SPARQL that parameters cannot be put into yet.".to_string()),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(name: &str, default: Option<&str>) -> Param {
        Param { name: name.into(), kind: Kind::String, default: default.map(str::to_string) }
    }

    fn pk(name: &str, kind: Kind) -> Param {
        Param { name: name.into(), kind, default: None }
    }

    fn given(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    const Q: &str = "SELECT ?d WHERE { ?b <http://e/name> ?being ; <http://e/desc> ?d }";

    #[test]
    fn a_query_document_declares_its_parameters_in_frontmatter() {
        let fm = "type: Query\ncopia.Query.parameter:\n  - \"being=sylkie\"\n  - \"set\"\n";
        assert_eq!(declared(Some(fm)).unwrap(), vec![p("being", Some("sylkie")), p("set", None)]);
        assert_eq!(declared(Some("soul.Query.parameter: \"n=5\"\n")).unwrap(), vec![p("n", Some("5"))]);
        let kinds = "a.Query.parameter:\n  - \"count:integer=10\"\n  - \"since:date\"\n  - \"doc:iri=<soul/Query/recent>\"\n";
        assert_eq!(
            declared(Some(kinds)).unwrap(),
            vec![
                Param { name: "count".into(), kind: Kind::Integer, default: Some("10".into()) },
                pk("since", Kind::Date),
                Param { name: "doc".into(), kind: Kind::Iri, default: Some("<soul/Query/recent>".into()) },
            ]
        );
        assert!(declared(Some("a.Query.parameter: \"n:number\"\n")).unwrap_err().contains("The kinds are: string, integer"));
        assert!(declared(Some("a.Query.parameter: \"n:integer=ten\"\n")).unwrap_err().contains("the default is wrong"));
        assert_eq!(declared(Some("type: Query\n")).unwrap(), vec![]);
        assert_eq!(declared(None).unwrap(), vec![]);
        assert!(declared(Some("a.Query.parameter:\n  - \"x\"\n  - \"x=1\"\n")).unwrap_err().contains("twice"));
        assert!(declared(Some("a.Query.parameter: \"two words\"\n")).unwrap_err().contains("letters, digits"));
    }

    #[test]
    fn a_query_with_no_parameters_runs_word_for_word() {
        let text = "SELECT * WHERE { ?s ?p ?o } # keep me";
        assert_eq!(bind(None, text, &[], &[]).unwrap(), text);
    }

    #[test]
    fn a_value_goes_in_as_a_value_and_a_default_fills_a_gap() {
        let params = [p("being", Some("sylkie"))];
        let with_default = bind(None, Q, &params, &[]).unwrap();
        assert!(with_default.contains("\"sylkie\""), "{with_default}");
        assert!(!with_default.contains("?being"), "{with_default}");
        let overridden = bind(None, Q, &params, &given(&[("being", "sylkie-glitch")])).unwrap();
        assert!(overridden.contains("\"sylkie-glitch\""), "{overridden}");
    }

    #[test]
    fn nothing_a_caller_writes_can_change_the_question() {
        // The classic: close the quote, close the group, ask for everything.
        let attack = "x\" } UNION { ?s ?p ?o } #";
        let out = bind(None, Q, &[p("being", None)], &given(&[("being", attack)])).unwrap();
        let parsed = spargebra::SparqlParser::new().parse_query(&out).unwrap();
        // Read back, the query still has one pattern and no UNION: the whole
        // of what was written arrived as one piece of text.
        assert!(!format!("{parsed:?}").contains("Union"), "{parsed:?}");
        assert!(out.contains("\"x\\\" } UNION { ?s ?p ?o } #\""), "{out}");
    }

    #[test]
    fn a_missing_or_unknown_parameter_is_named_never_run_empty() {
        let params = [p("being", Some("sylkie")), p("set", None)];
        let q = "SELECT ?d WHERE { ?b <http://e/name> ?being ; <http://e/set> ?set ; <http://e/desc> ?d }";
        let missing = bind(None, q, &params, &[]).unwrap_err();
        assert!(missing.contains("'set' is missing") && missing.contains("set=value"), "{missing}");
        let unknown = bind(None, q, &params, &given(&[("sett", "1")])).unwrap_err();
        assert!(unknown.contains("no parameter named 'sett'") && unknown.contains("being (default sylkie)"), "{unknown}");
        let none = bind(None, Q, &[], &given(&[("being", "x")])).unwrap_err();
        assert!(none.contains("It takes none."), "{none}");
        let unused = bind(None, "SELECT * WHERE { ?s ?p ?o }", &[p("being", Some("x"))], &[]).unwrap_err();
        assert!(unused.contains("never uses it"), "{unused}");
    }

    #[test]
    fn a_value_is_read_as_the_kind_declared_and_nothing_is_guessed() {
        let kind = |k: Kind, raw: &str| match read_value("x", k, raw).unwrap() {
            Value::Iri(n) => format!("<{}>", n.as_str()),
            Value::Literal(l) => format!("{}^^{}", l.value(), l.datatype().as_str().rsplit('#').next().unwrap()),
        };
        // With no kind, everything is text, whatever it looks like.
        for looks_like_something in ["sylkie", "42", "true", "2026-11-17", "<a/b>", "\"quoted\""] {
            assert_eq!(kind(Kind::String, looks_like_something), format!("{looks_like_something}^^string"));
        }
        assert_eq!(kind(Kind::Integer, "-42"), "-42^^integer");
        assert_eq!(kind(Kind::Decimal, "1.5"), "1.5^^decimal");
        assert_eq!(kind(Kind::Decimal, "2"), "2^^decimal");
        assert_eq!(kind(Kind::Boolean, "true"), "true^^boolean");
        assert_eq!(kind(Kind::Date, "2026-11-17"), "2026-11-17^^date");
        assert_eq!(kind(Kind::DateTime, "2026-11-17T09:00:00-08:00"), "2026-11-17T09:00:00-08:00^^dateTime");
        assert_eq!(kind(Kind::DateTime, "2026-11-17T09:00:00.25Z"), "2026-11-17T09:00:00.25Z^^dateTime");
        assert_eq!(kind(Kind::Iri, "<soul/Query/recent>"), "<https://repolex.ai/soul/Query/recent>");
        assert_eq!(kind(Kind::Iri, "soul/Query/recent"), "<https://repolex.ai/soul/Query/recent>");
        assert_eq!(kind(Kind::Iri, "http://e/x"), "<http://e/x>");
        // A value that does not fit its kind is refused, saying what fits.
        for (k, bad, takes) in [
            (Kind::Integer, "abc", "a whole number"),
            (Kind::Integer, "1.5", "a whole number"),
            (Kind::Decimal, "1.", "a number"),
            (Kind::Boolean, "yes", "true or false"),
            (Kind::Date, "2026-13-01", "a date"),
            (Kind::Date, "tomorrow", "a date"),
            (Kind::DateTime, "2026-11-17", "a date and time"),
            (Kind::DateTime, "2026-11-17T9am", "a date and time"),
            (Kind::Iri, "two words", "a document's address"),
        ] {
            let err = read_value("x", k, bad).unwrap_err();
            assert!(err.contains(takes) && err.contains(bad), "{err}");
        }
    }

    #[test]
    fn an_id_that_looks_like_a_number_still_finds_its_text() {
        // The case that made kinds the author's to state: a Being named 42.
        let out = bind(None, Q, &[p("being", None)], &given(&[("being", "42")])).unwrap();
        assert!(out.contains("<http://e/name> \"42\""), "{out}");
        let counted = "SELECT ?d WHERE { ?b <http://e/n> ?being ; <http://e/desc> ?d }";
        let out = bind(None, counted, &[pk("being", Kind::Integer)], &given(&[("being", "42")])).unwrap();
        assert!(out.contains("<http://e/n> \"42\"^^<http://www.w3.org/2001/XMLSchema#integer>"), "{out}");
        let err = bind(None, counted, &[pk("being", Kind::Integer)], &given(&[("being", "x")])).unwrap_err();
        assert!(err.contains("'being' takes a whole number"), "{err}");
    }

    #[test]
    fn a_parameter_reaches_every_place_the_query_uses_it() {
        // Inside a filter, an optional part, a sub-query and a service call,
        // and it keeps its column when the query returns it.
        let q = "SELECT ?being ?d WHERE { \
                   SERVICE <http://svc/> { ?b <http://e/name> ?being } \
                   OPTIONAL { ?b <http://e/desc> ?d FILTER(?d != ?being) } \
                   { SELECT ?b WHERE { ?b <http://e/alias> ?being } } \
                   FILTER NOT EXISTS { ?b <http://e/hidden> ?being } }";
        let out = bind(None, q, &[p("being", None)], &given(&[("being", "sylkie")])).unwrap();
        assert_eq!(out.matches("\"sylkie\"").count(), 5, "{out}");
        assert_eq!(out.matches("?being").count(), 1, "only the returned column: {out}");
        assert!(out.contains("(\"sylkie\" AS ?being)"), "{out}");
    }

    #[test]
    fn a_value_that_cannot_go_where_the_query_uses_it_is_refused() {
        let as_property = "SELECT ?o WHERE { ?s ?prop ?o }";
        let err = bind(None, as_property, &[p("prop", None)], &given(&[("prop", "name")])).unwrap_err();
        assert!(err.contains("as a property") && err.contains("prop:iri"), "{err}");
        assert!(bind(None, as_property, &[pk("prop", Kind::Iri)], &given(&[("prop", "<http://e/name>")])).unwrap().contains("<http://e/name>"));
        let sets = "SELECT ?x WHERE { ?s ?p ?o BIND(1 AS ?x) }";
        assert!(bind(None, sets, &[p("x", None)], &given(&[("x", "2")])).unwrap_err().contains("gives ?x a value itself"));
    }
}
