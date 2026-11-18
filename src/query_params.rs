//! Parameters on saved queries (goodlux, 2026-11-17).
//!
//! A saved query declares what it takes in its frontmatter, under its class's
//! `parameter` property: one value per parameter, `name` (must be given) or
//! `name=default`. The query text uses it as the variable `?name`.
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
const ADDRESS_ROOT: &str = "https://repolex.ai/";

const XSD: &str = "http://www.w3.org/2001/XMLSchema#";

/// One declared parameter.
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: String,
    pub default: Option<String>,
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
            let (name, default) = match entry.split_once('=') {
                Some((n, d)) => (n.trim().to_string(), Some(d.trim().to_string())),
                None => (entry.trim().to_string(), None),
            };
            let name = name.strip_prefix('?').unwrap_or(&name).to_string();
            if !valid_name(&name) {
                return Err(format!(
                    "`{key}` declares a parameter named '{name}'. A name is letters, digits and underscores, starting with a letter."
                ));
            }
            if out.iter().any(|p| p.name == name) {
                return Err(format!("`{key}` declares the parameter '{name}' twice."));
            }
            out.push(Param { name, default });
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
    b.len() == 10 && b[4] == b'-' && b[7] == b'-' && is_digits(&s[..4]) && is_digits(&s[5..7]) && is_digits(&s[8..])
}

/// Read a value from how it is written:
///
/// - `<...>` is a document's address. One without a scheme
///   (`<soul/Query/recent>`) is the short form used in frontmatter.
/// - a whole number, a decimal, `true`/`false`, a date (`2026-11-17`) or a
///   date and time (`2026-11-17T09:00:00-08:00`) is that kind of value.
/// - anything else is text. Text in double quotes is text whatever it looks
///   like, so `"42"` is the text 42, not the number.
pub fn read_value(name: &str, raw: &str) -> Result<Value, String> {
    if let Some(inner) = raw.strip_prefix('<').and_then(|r| r.strip_suffix('>')) {
        let full = if inner.contains("://") || inner.starts_with("urn:") { inner.to_string() } else { format!("{ADDRESS_ROOT}{inner}") };
        return NamedNode::new(&full).map(Value::Iri).map_err(|e| format!("The value of '{name}', {raw}, is not a usable address: {e}"));
    }
    if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
        return Ok(Value::Literal(Literal::new_simple_literal(&raw[1..raw.len() - 1])));
    }
    let unsigned = raw.strip_prefix('-').unwrap_or(raw);
    if is_digits(unsigned) {
        return Ok(typed(raw, "integer"));
    }
    if let Some((whole, fraction)) = unsigned.split_once('.')
        && is_digits(whole)
        && is_digits(fraction)
    {
        return Ok(typed(raw, "decimal"));
    }
    if raw == "true" || raw == "false" {
        return Ok(typed(raw, "boolean"));
    }
    if is_date(raw) {
        return Ok(typed(raw, "date"));
    }
    if raw.len() > 11 && raw.is_char_boundary(10) && is_date(&raw[..10]) && raw.as_bytes()[10] == b'T' {
        return Ok(typed(raw, "dateTime"));
    }
    Ok(Value::Literal(Literal::new_simple_literal(raw)))
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
        .map(|p| match &p.default {
            Some(d) => format!("{} (default {d})", p.name),
            None => format!("{} (must be given)", p.name),
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
    let mut values: Vec<(Variable, Value)> = Vec::new();
    for p in declared {
        let raw = given.iter().find(|(n, _)| n == &p.name).map(|(_, v)| v.as_str()).or(p.default.as_deref());
        let Some(raw) = raw else {
            return Err(format!("The parameter '{0}' is missing and has no default. Give it as {0}=value. {1}", p.name, describe(declared)));
        };
        values.push((Variable::new_unchecked(p.name.clone()), read_value(&p.name, raw)?));
    }
    let with_prefixes = crate::add_prefixes_at(root, text);
    let mut query = spargebra::SparqlParser::new().parse_query(&with_prefixes).map_err(|e| format!("The query does not parse: {e}"))?;
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
        format!("The query uses ?{0} as {place}, so '{0}' must be a document's address, written <...>.", v.as_str())
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
        Param { name: name.into(), default: default.map(str::to_string) }
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
    fn a_value_is_read_from_how_it_is_written() {
        let kind = |raw: &str| match read_value("x", raw).unwrap() {
            Value::Iri(n) => format!("<{}>", n.as_str()),
            Value::Literal(l) => format!("{}^^{}", l.value(), l.datatype().as_str().rsplit('#').next().unwrap()),
        };
        assert_eq!(kind("sylkie"), "sylkie^^string");
        assert_eq!(kind("42"), "42^^integer");
        assert_eq!(kind("\"42\""), "42^^string");
        assert_eq!(kind("-1.5"), "-1.5^^decimal");
        assert_eq!(kind("true"), "true^^boolean");
        assert_eq!(kind("2026-11-17"), "2026-11-17^^date");
        assert_eq!(kind("2026-11-17T09:00:00-08:00"), "2026-11-17T09:00:00-08:00^^dateTime");
        assert_eq!(kind("<soul/Query/recent>"), "<https://repolex.ai/soul/Query/recent>");
        assert_eq!(kind("<http://e/x>"), "<http://e/x>");
        assert_eq!(kind("1.2.3"), "1.2.3^^string");
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
        assert!(err.contains("as a property") && err.contains("<...>"), "{err}");
        assert!(bind(None, as_property, &[p("prop", None)], &given(&[("prop", "<http://e/name>")])).unwrap().contains("<http://e/name>"));
        let sets = "SELECT ?x WHERE { ?s ?p ?o BIND(1 AS ?x) }";
        assert!(bind(None, sets, &[p("x", None)], &given(&[("x", "2")])).unwrap_err().contains("gives ?x a value itself"));
    }
}
