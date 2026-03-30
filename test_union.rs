use oxigraph::sparql::Query;
fn test() {
    let mut q = Query::parse("SELECT * WHERE {?s ?p ?o}", None).unwrap();
    q.dataset_mut().set_default_graph_as_union();
}
