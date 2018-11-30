use std::ops::{Deref, Range};
use std::{mem, vec, str};
use std::error::Error;
use std::hash::Hash;

use fnv::FnvHashMap;
use fst::Streamer;
use group_by::GroupByMut;
use ::rocksdb::rocksdb::{DB, Snapshot};

use crate::automaton::{self, DfaExt, AutomatonExt};
use crate::rank::criterion::{self, Criterion};
use crate::rank::distinct_map::DistinctMap;
use crate::blob::PositiveBlob;
use crate::{Match, DocumentId};
use crate::retrieve::Retrieve;
use crate::rank::Document;

fn clamp_range<T: Copy + Ord>(range: Range<T>, big: Range<T>) -> Range<T> {
    Range {
        start: range.start.min(big.end).max(big.start),
        end: range.end.min(big.end).max(big.start),
    }
}

fn split_whitespace_automatons(query: &str) -> Vec<DfaExt> {
    let mut automatons = Vec::new();
    for query in query.split_whitespace().map(str::to_lowercase) {
        let lev = automaton::build_prefix_dfa(&query);
        automatons.push(lev);
    }
    automatons
}

pub struct QueryBuilder<T: Deref<Target=DB>, C> {
    snapshot: Snapshot<T>,
    blob: PositiveBlob,
    criteria: Vec<C>,
}

impl<T: Deref<Target=DB>> QueryBuilder<T, Box<dyn Criterion>> {
    pub fn new(snapshot: Snapshot<T>) -> Result<Self, Box<Error>> {
        QueryBuilder::with_criteria(snapshot, criterion::default())
    }
}

impl<T, C> QueryBuilder<T, C>
where T: Deref<Target=DB>,
{
    pub fn with_criteria(snapshot: Snapshot<T>, criteria: Vec<C>) -> Result<Self, Box<Error>> {
        let blob = snapshot.data_index()?;
        Ok(QueryBuilder { snapshot, blob, criteria })
    }

    pub fn criteria(&mut self, criteria: Vec<C>) -> &mut Self {
        self.criteria = criteria;
        self
    }

    pub fn with_distinct<F>(self, function: F, size: usize) -> DistinctQueryBuilder<T, F, C> {
        DistinctQueryBuilder {
            inner: self,
            function: function,
            size: size
        }
    }

    fn query_all(&self, query: &str) -> Vec<Document> {
        let automatons = split_whitespace_automatons(query);

        let mut stream = {
            let mut op_builder = fst::map::OpBuilder::new();
            for automaton in &automatons {
                let stream = self.blob.as_map().search(automaton);
                op_builder.push(stream);
            }
            op_builder.union()
        };

        let mut matches = FnvHashMap::default();

        while let Some((input, indexed_values)) = stream.next() {
            for iv in indexed_values {
                let automaton = &automatons[iv.index];
                let distance = automaton.eval(input).to_u8();
                let is_exact = distance == 0 && input.len() == automaton.query_len();

                let doc_indexes = self.blob.as_indexes();
                let doc_indexes = doc_indexes.get(iv.value).expect("BUG: could not find document indexes");

                for doc_index in doc_indexes {
                    let match_ = Match {
                        query_index: iv.index as u32,
                        distance: distance,
                        attribute: doc_index.attribute,
                        attribute_index: doc_index.attribute_index,
                        is_exact: is_exact,
                    };
                    matches.entry(doc_index.document_id).or_insert_with(Vec::new).push(match_);
                }
            }
        }

        matches.into_iter().map(|(id, matches)| Document::from_matches(id, matches)).collect()
    }
}

impl<T, C> QueryBuilder<T, C>
where T: Deref<Target=DB>,
      C: Criterion,
{
    pub fn query(&self, query: &str, range: Range<usize>) -> Vec<Document> {
        let mut documents = self.query_all(query);
        let mut groups = vec![documents.as_mut_slice()];

        for criterion in &self.criteria {
            let tmp_groups = mem::replace(&mut groups, Vec::new());

            for group in tmp_groups {
                group.sort_unstable_by(|a, b| criterion.evaluate(a, b));
                for group in GroupByMut::new(group, |a, b| criterion.eq(a, b)) {
                    groups.push(group);
                }
            }
        }

        let range = clamp_range(range, 0..documents.len());
        documents[range].to_vec()
    }
}

pub struct DistinctQueryBuilder<T: Deref<Target=DB>, F, C> {
    inner: QueryBuilder<T, C>,
    function: F,
    size: usize,
}

pub struct DocDatabase;

impl<T: Deref<Target=DB>, F, K, C> DistinctQueryBuilder<T, F, C>
where T: Deref<Target=DB>,
      F: Fn(DocumentId, &DocDatabase) -> Option<K>,
      K: Hash + Eq,
      C: Criterion,
{
    pub fn query(&self, query: &str, range: Range<usize>) -> Vec<Document> {
        let mut documents = self.inner.query_all(query);
        let mut groups = vec![documents.as_mut_slice()];

        for criterion in &self.inner.criteria {
            let tmp_groups = mem::replace(&mut groups, Vec::new());

            for group in tmp_groups {
                group.sort_unstable_by(|a, b| criterion.evaluate(a, b));
                for group in GroupByMut::new(group, |a, b| criterion.eq(a, b)) {
                    groups.push(group);
                }
            }
        }

        let doc_database = DocDatabase;
        let mut out_documents = Vec::with_capacity(range.len());
        let mut seen = DistinctMap::new(self.size);

        for document in documents {
            let accepted = match (self.function)(document.id, &doc_database) {
                Some(key) => seen.digest(key),
                None => seen.accept_without_key(),
            };

            if accepted {
                if seen.len() == range.end { break }
                if seen.len() >= range.start {
                    out_documents.push(document);
                }
            }
        }

        out_documents
    }
}