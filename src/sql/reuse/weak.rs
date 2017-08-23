use sql::reuse::helpers::predicate_implication::complex_predicate_implies;
use sql::reuse::{ReuseConfiguration, ReuseType};
use sql::query_graph::{QueryGraph, QueryGraphEdge};
use mir::MirQuery;

use std::vec::Vec;
use std::collections::HashMap;


/// Implementation of reuse algorithm with weaker constraints than Finkelstein.
/// While Finkelstein checks if queries are compatible for direct extension,
/// this algorithm considers the possibility of reuse of internal views.
/// For example, given the queries:
/// 1) select * from Paper, PaperReview where Paper.paperId = PaperReview.paperId and PaperReview.reviewType = 1;
/// 2) select * from Paper, PaperReview where Paper.paperId = PaperReview.paperId;
///
/// Finkelstein reuse would be conditional on the order the queries are added,
/// since 1) is a direct extension of 2), but not the other way around.
///
/// This weak version of the reuse algorithm considers cases where a query might
/// reuse just a prefix of another. First, it checks if the queries perform the
/// same joins, then it checks predicate implication and at last, checks group
/// by compatibility.
/// If all checks pass, them the algorithm works like Finkelstein and the query
/// is a direct extension of the other. However, if not all, but at least one
/// check passes, then the algorithm returns that the queries have a prefix in
/// common.
pub struct Weak;

impl ReuseConfiguration for Weak {
    fn reuse_candidates<'a>(qg: &QueryGraph, query_graphs: &'a HashMap<u64, (QueryGraph, MirQuery)>) -> Vec<(ReuseType, &'a QueryGraph)>{
        let mut reuse_candidates = Vec::new();
        for &(ref existing_qg, _) in query_graphs.values() {
            if existing_qg
                .signature()
                .is_weak_generalization_of(&qg.signature())
            {
                match Self::check_compatibility(&qg, existing_qg) {
                    Some(reuse) => {
                        // QGs are compatible, we can reuse `existing_qg` as part of `qg`!
                        reuse_candidates.push((reuse, existing_qg));
                    }
                    None => (),
                }
            }
        }

        reuse_candidates
    }

    fn choose_best_option<'a>(options: Vec<(ReuseType, &'a QueryGraph)>) -> (ReuseType, &'a QueryGraph) {
        let mut best_choice = None;
        let mut best_score = 0;

        for (o, qg) in options {
            let mut score = 0;

            // crude scoring: direct extension always preferrable over backjoins; reusing larger
            // queries is also preferrable as they are likely to cover a larger fraction of the new
            // query's nodes. Edges (group by, join) count for more than extra relations.
            match o {
                ReuseType::DirectExtension => {
                    score += 2 * qg.relations.len() + 4 * qg.edges.len() + 1000;
                }
                ReuseType::PrefixReuse => {
                    score += 2 * qg.relations.len() + 4 * qg.edges.len() + 500;
                }
                ReuseType::BackjoinRequired(_) => {
                    score += qg.relations.len() + 3 * qg.edges.len();
                }
            }

            if score > best_score {
                best_score = score;
                best_choice = Some((o, qg));
            }
        }

        assert!(best_score > 0);

        best_choice.unwrap()
    }
}

impl Weak {
    fn check_compatibility(new_qg: &QueryGraph, existing_qg: &QueryGraph) -> Option<ReuseType> {
        // 1. NQG's nodes is subset of EQG's nodes
        // -- already established via signature check
        assert!(
            existing_qg
                .signature()
                .is_weak_generalization_of(&new_qg.signature())
        );

        // Check if the queries are join compatible -- if the new query
        // performs a superset of the joins in the existing query.
        for (srcdst, ex_qge) in &existing_qg.edges {
            match *ex_qge {
                QueryGraphEdge::Join(_) => {
                    if !new_qg.edges.contains_key(srcdst) { return None; }
                    let new_qge = &new_qg.edges[srcdst];
                    match *new_qge {
                        QueryGraphEdge::Join(_) => {}
                        // If there is no matching Join edge, we cannot reuse
                        _ => return None,
                    }
                }
                QueryGraphEdge::LeftJoin(_) => {
                    if !new_qg.edges.contains_key(srcdst) { return None; }
                    let new_qge = &new_qg.edges[srcdst];
                    match *new_qge {
                        QueryGraphEdge::LeftJoin(_) => {}
                        // If there is no matching LeftJoin edge, we cannot reuse
                        _ => return None,
                    }
                }
                _ => continue
            }
        }

        // Checks group by compatibility between queries.
        for (srcdst, ex_qge) in &existing_qg.edges {
            match *ex_qge {
                QueryGraphEdge::GroupBy(ref ex_columns) => {
                    if !new_qg.edges.contains_key(srcdst) { return Some(ReuseType::PrefixReuse); }
                    let new_qge = &new_qg.edges[srcdst];
                    match *new_qge {
                        QueryGraphEdge::GroupBy(ref new_columns) => {
                            // GroupBy implication holds if the new QG groups by the same columns as
                            // the original one, or by a *superset* (as we can always apply more
                            // grouped operatinos on top of earlier ones)
                            if new_columns.len() < ex_columns.len() {
                                // more columns in existing QG's GroupBy, so we're done
                                // however, we can still reuse joins and predicates.
                                return Some(ReuseType::PrefixReuse);
                            }
                            for ex_col in ex_columns {
                                // EQG groups by a column that we don't group by, so we can't reuse
                                // the group by nodes, but we can still reuse joins and predicates.
                                if !new_columns.contains(ex_col) {
                                    return Some(ReuseType::PrefixReuse);
                                }
                            }
                        }
                        // If there is no matching GroupBy edge, we cannot reuse the group by clause
                        _ => return Some(ReuseType::PrefixReuse),
                    }
                }
                _ => continue
            }
        }

        // Check that the new query's predicates imply the existing query's predicate.
        for (name, ex_qgn) in &existing_qg.relations {
            let new_qgn = &new_qg.relations[name];

            // iterate over predicates and ensure that each matching one on the existing QG is implied
            // by the new one
            for ep in &ex_qgn.predicates {
                let mut matched = false;

                for np in &new_qgn.predicates {
                    if complex_predicate_implies(np, ep) {
                        matched = true;
                        break
                    }
                }
                if !matched {
                    // We found no matching predicate for np, so we give up now.
                    // However, we can still reuse the join nodes from the existing query.
                    return Some(ReuseType::PrefixReuse);
                }
            }
        }

        // we don't need to check projected columns to reuse a prefix of the query
        return Some(ReuseType::DirectExtension);
    }
}