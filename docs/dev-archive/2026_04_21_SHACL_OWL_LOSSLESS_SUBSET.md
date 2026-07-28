# SHACL / OWL Lossless Subset

Reference for the subset of SHACL and OWL constructs that map 1:1 in both directions. Based on the Astrea mapping patterns (Cimmino et al., ESWC 2020) and Knublauch's SHACL/OWL comparison.

## Context

git-lex uses SHACL shapes as the primary schema input for frontmatter extraction and validation. Kit authors may optionally maintain OWL ontologies, but git-lex only reads shapes at runtime. This document defines the subset of constructs that are safe to use across both formats without information loss.

## Lossless Mappings (round-trip safe)

| OWL Construct | SHACL Construct | Notes |
|---|---|---|
| `owl:Class` | `sh:NodeShape` + `sh:targetClass` | Class declaration |
| `owl:DatatypeProperty` + `rdfs:range xsd:T` | `sh:property` + `sh:datatype xsd:T` | Typed literal property |
| `owl:ObjectProperty` | `sh:property` + `sh:nodeKind sh:IRI` | IRI reference property |
| `rdfs:range` (class) | `sh:class` | Range is a class, not a datatype |
| `rdfs:domain` | Determines which `sh:NodeShape` owns the `sh:property` | Structural inversion |
| `owl:minCardinality N` | `sh:minCount N` | Minimum cardinality |
| `owl:maxCardinality N` | `sh:maxCount N` | Maximum cardinality |
| `owl:cardinality N` | `sh:minCount N` + `sh:maxCount N` | Exact cardinality |
| `owl:oneOf` (on `rdfs:Datatype`) | `sh:in (...)` | Enumerated values |
| `owl:hasValue` | `sh:hasValue` | Fixed value constraint |
| `owl:FunctionalProperty` | `sh:maxCount 1` | At most one value |
| `owl:unionOf` | `sh:or` | Disjunction |
| `owl:intersectionOf` | `sh:and` | Conjunction |
| `owl:complementOf` | `sh:not` | Negation |

## OWL constructs with NO SHACL equivalent

These are lost in OWL-to-SHACL conversion:

- `rdfs:subClassOf` — class hierarchy / inheritance
- `owl:equivalentClass` / `owl:sameAs` — class/instance equivalence
- `owl:disjointWith` / `owl:AllDisjointClasses` — disjoint sets
- `owl:TransitiveProperty`, `owl:SymmetricProperty`, `owl:ReflexiveProperty` — property characteristics
- `owl:inverseOf` — inverse property declarations
- `owl:propertyChainAxiom` — property chains

## SHACL constructs with NO OWL equivalent

These are lost in SHACL-to-OWL conversion:

- `sh:pattern` — regex constraints
- `sh:languageIn` — language tag constraints
- `sh:uniqueLang` — unique language per property
- Complex `sh:path` expressions (inverse, sequence, alternation)
- `sh:qualifiedValueShape` with qualified cardinality
- `sh:sparql` — arbitrary SPARQL constraints
- `sh:closed` — closed-world assertion
- `sh:order`, `sh:group`, `sh:name`, `sh:description` — UI metadata
- `sh:severity`, `sh:message` — validation messaging
- `sh:deactivated` — shape activation control
- `dash:` extensions (reification shapes, etc.)

## git-lex usage

git-lex currently uses only the lossless subset:

| Purpose | SHACL construct used |
|---|---|
| Class identification | `sh:targetClass` |
| Property declaration | `sh:property` + `sh:path` |
| IRI vs literal | `sh:nodeKind sh:IRI` |
| Typed literals | `sh:datatype xsd:integer`, etc. |
| Required fields | `sh:minCount 1` |
| Enum values | `sh:in (...)` |

This means git-lex's schema requirements are fully expressible in either SHACL or OWL without loss. Kit authors can write shapes directly (recommended) or generate them from OWL using tools like Astrea or owl2shacl.

## Key insight: SHACL is per-node, OWL is per-graph

SHACL shapes describe the structure of individual nodes — what properties a node should have, what types, what constraints. They do not express relationships between classes (hierarchy, equivalence, disjointness).

OWL ontologies describe the structural relationships of a graph — class hierarchies, property characteristics, inference rules.

For git-lex's job (read one markdown file, extract typed RDF, validate), SHACL shapes are the right tool. Cross-kit reasoning and class hierarchies are application-layer concerns that live outside git-lex.

## References

- Cimmino et al., "Astrea: Automatic Generation of SHACL Shapes from Ontologies" (ESWC 2020)
- Knublauch, "SHACL and OWL Compared" — spinrdf.org/shacl-and-owl.html
- Knublauch, "Why I Use SHACL For Defining Ontology Models" — topquadrant.com
- "Lessons Learned from the Combined Development of OWL and SHACL" (K-CAP 2025)
