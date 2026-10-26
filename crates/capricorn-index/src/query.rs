use std::ops::Bound;

use capricorn_core::proto::{Kind, SearchQuery, Ts};
use tantivy::Term;
use tantivy::query::{
    AllQuery, BooleanQuery, BoostQuery, ConstScoreQuery, FuzzyTermQuery, Occur, Query, RangeQuery,
    TermQuery,
};
use tantivy::schema::{Field, IndexRecordOption};
use tantivy::tokenizer::TextAnalyzer;

use crate::highlight::fuzzy_distance;
use crate::schema::Fields;

pub(crate) fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Screen => "screen",
        Kind::Clipboard => "clipboard",
    }
}

pub(crate) fn parse_kind(name: &str) -> Kind {
    match name {
        "clipboard" => Kind::Clipboard,
        _ => Kind::Screen,
    }
}

pub(crate) fn tokenize(analyzer: &TextAnalyzer, text: &str) -> Vec<String> {
    let mut analyzer = analyzer.clone();
    let mut stream = analyzer.token_stream(text);
    let mut tokens = Vec::new();
    while stream.advance() {
        tokens.push(stream.token().text.clone());
    }
    tokens
}

pub(crate) fn search_query(fields: &Fields, terms: &[String], q: &SearchQuery) -> Box<dyn Query> {
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = terms
        .iter()
        .map(|term| (Occur::Must, term_clause(fields, term)))
        .collect();
    clauses.extend(
        filters(fields, q)
            .into_iter()
            .map(|filter| (Occur::Must, filter)),
    );
    match clauses.len() {
        0 => Box::new(AllQuery),
        _ => Box::new(BooleanQuery::new(clauses)),
    }
}

fn filters(fields: &Fields, q: &SearchQuery) -> Vec<Box<dyn Query>> {
    let mut filters: Vec<Box<dyn Query>> = Vec::new();
    if let Some(kind) = q.kind {
        filters.push(exact(fields.kind, kind_name(kind)));
    }
    if let Some(app) = &q.app {
        filters.push(exact(fields.app_key, &app.to_lowercase()));
    }
    if let Some(workspace) = &q.workspace {
        filters.push(exact(fields.workspace, workspace));
    }
    if q.from.is_some() || q.to.is_some() {
        filters.push(Box::new(ts_range(fields, q.from, q.to)));
    }
    filters
        .into_iter()
        .map(|filter| Box::new(ConstScoreQuery::new(filter, 0.0)) as Box<dyn Query>)
        .collect()
}

fn exact(field: Field, value: &str) -> Box<dyn Query> {
    Box::new(TermQuery::new(
        Term::from_field_text(field, value),
        IndexRecordOption::Basic,
    ))
}

pub(crate) fn exact_u64(field: Field, value: u64) -> Box<dyn Query> {
    Box::new(TermQuery::new(
        Term::from_field_u64(field, value),
        IndexRecordOption::Basic,
    ))
}

pub(crate) fn kind_query(fields: &Fields, kind: Kind) -> Box<dyn Query> {
    exact(fields.kind, kind_name(kind))
}

pub(crate) fn app_query(fields: &Fields, class: &str) -> Box<dyn Query> {
    exact(fields.app_key, &class.to_lowercase())
}

/// Both bounds inclusive.
pub(crate) fn ts_range(fields: &Fields, from: Option<Ts>, to: Option<Ts>) -> RangeQuery {
    let bound = |value: Option<Ts>| match value {
        Some(ts) => Bound::Included(Term::from_field_i64(fields.ts, ts)),
        None => Bound::Unbounded,
    };
    RangeQuery::new(bound(from), bound(to))
}

pub(crate) fn all_of(queries: Vec<Box<dyn Query>>) -> Box<dyn Query> {
    Box::new(BooleanQuery::new(
        queries.into_iter().map(|q| (Occur::Must, q)).collect(),
    ))
}

fn term_clause(fields: &Fields, token: &str) -> Box<dyn Query> {
    let searched = [
        (fields.body, 1.0),
        (fields.title, 2.0),
        (fields.app, 2.0),
        (fields.cwd, 1.0),
        (fields.url, 1.0),
    ];
    let mut alternatives: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    for (field, boost) in searched {
        let term = Term::from_field_text(field, token);
        let exact = TermQuery::new(term.clone(), IndexRecordOption::WithFreqs);
        let lenient = FuzzyTermQuery::new_prefix(term, fuzzy_distance(token), true);
        alternatives.push((
            Occur::Should,
            Box::new(BoostQuery::new(Box::new(exact), boost)),
        ));
        alternatives.push((Occur::Should, Box::new(lenient)));
    }
    Box::new(BooleanQuery::new(alternatives))
}
