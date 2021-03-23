use crate::controller::sql::mir::SqlToMirConverter;
use crate::controller::sql::query_graph::{QueryGraph, QueryGraphEdge};
use crate::controller::sql::query_utils::{function_arguments, is_aggregate, ReferredColumns};
use crate::ReadySetResult;
use crate::{internal, invariant, unsupported};
use mir::{Column, MirNodeRef};
use nom_sql::{self, ConditionExpression, FunctionArgument, FunctionExpression};
use nom_sql::{Expression, FunctionExpression::*};
use std::collections::{HashMap, HashSet};
use std::ops::Deref;

// Move predicates above grouped_by nodes
pub(super) fn make_predicates_above_grouped<'a>(
    mir_converter: &SqlToMirConverter,
    name: &str,
    qg: &QueryGraph,
    node_for_rel: &HashMap<&str, MirNodeRef>,
    node_count: usize,
    column_to_predicates: &HashMap<Column, Vec<&'a ConditionExpression>>,
    prev_node: &mut Option<MirNodeRef>,
) -> ReadySetResult<(Vec<&'a ConditionExpression>, Vec<MirNodeRef>)> {
    let mut created_predicates = Vec::new();
    let mut predicates_above_group_by_nodes = Vec::new();
    let mut node_count = node_count;

    if let Some(computed_cols_cgn) = qg.relations.get("computed_columns") {
        for ccol in &computed_cols_cgn.columns {
            // whenever we have a column getting aggregated (i.e. an over column
            // rather than a group by column) we won't be able to filter on it
            // later, so any filters involving it need to get moved above
            for over_col in
                Expression::Call(ccol.function.as_deref().unwrap().clone()).referred_columns()
            {
                let over_table = over_col.as_ref().table.as_ref().unwrap().as_str();
                let col = Column::from(over_col.clone().into_owned());

                if column_to_predicates.contains_key(&col) {
                    let parent = match *prev_node {
                        Some(ref p) => p.clone(),
                        None => node_for_rel[over_table].clone(),
                    };

                    let new_mpns = mir_converter.predicates_above_group_by(
                        &format!("{}_n{}", name, node_count),
                        &column_to_predicates,
                        col,
                        parent,
                        &mut created_predicates,
                    )?;

                    node_count += predicates_above_group_by_nodes.len();
                    *prev_node = Some(new_mpns.last().unwrap().clone());
                    predicates_above_group_by_nodes.extend(new_mpns);
                }
            }
        }
    }

    Ok((created_predicates, predicates_above_group_by_nodes))
}

/// Normally, projection happens after grouped nodes - however, if aggregates used in grouped
/// expressions reference expressions rather than columns directly, we need to
pub(super) fn make_expressions_above_grouped(
    mir_converter: &SqlToMirConverter,
    name: &str,
    qg: &QueryGraph,
    node_count: usize,
    prev_node: &mut Option<MirNodeRef>,
) -> Option<(Vec<(String, Expression)>, MirNodeRef)> {
    let exprs: Vec<_> = qg
        .relations
        .get("computed_columns")
        .iter()
        .flat_map(|cgn| &cgn.columns)
        .filter_map(|c| c.function.as_ref())
        .filter(|f| is_aggregate(&f))
        .flat_map(|f| function_arguments(f))
        .filter_map(|arg| match arg {
            FunctionArgument::Column(c) => c.function.as_ref().map(|f| (c.name.clone(), f)),
            _ => None,
        })
        .map(|(n, f)| (n, Expression::Call((**f).clone())))
        .collect();

    if !exprs.is_empty() {
        let cols = prev_node.as_ref().unwrap().borrow().columns.to_vec();

        let node = mir_converter.make_project_node(
            &format!("{}_n{}", name, node_count),
            prev_node.clone().unwrap(),
            cols.iter().collect(),
            exprs.clone(),
            vec![],
            false,
        );
        *prev_node = Some(node.clone());
        Some((exprs, node))
    } else {
        None
    }
}

pub(super) fn make_grouped(
    mir_converter: &SqlToMirConverter,
    name: &str,
    qg: &QueryGraph,
    node_for_rel: &HashMap<&str, MirNodeRef>,
    node_count: usize,
    prev_node: &mut Option<MirNodeRef>,
    is_reconcile: bool,
) -> ReadySetResult<Vec<MirNodeRef>> {
    let mut func_nodes: Vec<MirNodeRef> = Vec::new();
    let mut node_count = node_count;

    if let Some(computed_cols_cgn) = qg.relations.get("computed_columns") {
        let gb_edges: Vec<_> = qg
            .edges
            .values()
            .filter(|e| match **e {
                QueryGraphEdge::Join(_) | QueryGraphEdge::LeftJoin(_) => false,
                QueryGraphEdge::GroupBy(_) => true,
            })
            .collect();

        for computed_col in computed_cols_cgn.columns.iter() {
            let computed_col = if is_reconcile {
                let func = computed_col.function.as_ref().unwrap();
                let new_func = match *func.deref() {
                    Sum(FunctionArgument::Column(ref col), b) => {
                        let colname = format!("{}.sum({})", col.table.as_ref().unwrap(), col.name);
                        FunctionExpression::Sum(
                            FunctionArgument::Column(nom_sql::Column::from(colname.as_ref())),
                            b,
                        )
                    }
                    Count(FunctionArgument::Column(ref col), b) => {
                        let colname = format!("{}.count({})", col.clone().table.unwrap(), col.name);
                        FunctionExpression::Sum(
                            FunctionArgument::Column(nom_sql::Column::from(colname.as_ref())),
                            b,
                        )
                    }
                    Max(FunctionArgument::Column(ref col)) => {
                        let colname = format!("{}.max({})", col.clone().table.unwrap(), col.name);
                        FunctionExpression::Max(FunctionArgument::Column(nom_sql::Column::from(
                            colname.as_ref(),
                        )))
                    }
                    Min(FunctionArgument::Column(ref col)) => {
                        let colname = format!("{}.min({})", col.clone().table.unwrap(), col.name);
                        FunctionExpression::Min(FunctionArgument::Column(nom_sql::Column::from(
                            colname.as_ref(),
                        )))
                    }
                    ref x => unsupported!("unknown function expression: {:?}", x),
                };

                nom_sql::Column {
                    function: Some(Box::new(new_func)),
                    name: computed_col.name.clone(),
                    alias: computed_col.alias.clone(),
                    table: computed_col.table.clone(),
                }
            } else {
                computed_col.clone()
            };

            // We must also push parameter columns through the group by
            let call_expr = Expression::Call(computed_col.function.as_deref().unwrap().clone());
            let mut over_cols = call_expr.referred_columns().peekable();

            let parent_node = match *prev_node {
                // If no explicit parent node is specified, we extract
                // the base node from the "over" column's specification
                None => {
                    // If we don't have a parent node yet, that means no joins or unions can
                    // have happened yet, which means there *must* only be one table referred in
                    // the aggregate expression. Let's just take the first.
                    node_for_rel[over_cols.peek().unwrap().table.as_ref().unwrap().as_str()].clone()
                }
                // We have an explicit parent node (likely a projection
                // helper), so use that
                Some(ref node) => node.clone(),
            };

            let name = &format!("{}_n{}", name, node_count);

            let (parent_node, group_cols) = if !gb_edges.is_empty() {
                // Function columns with GROUP BY clause
                let mut gb_cols: Vec<&nom_sql::Column> = Vec::new();

                for e in &gb_edges {
                    match **e {
                        QueryGraphEdge::GroupBy(ref gbc) => {
                            let table = gbc.first().unwrap().table.as_ref().unwrap();
                            invariant!(gbc.iter().all(|c| c.table.as_ref().unwrap() == table));
                            gb_cols.extend(gbc);
                        }
                        _ => internal!(),
                    }
                }

                // get any parameter columns that aren't also in the group-by
                // column set
                let param_cols: Vec<_> = qg.relations.values().fold(vec![], |acc, rel| {
                    acc.into_iter()
                        .chain(
                            rel.parameters
                                .iter()
                                .map(|(col, _)| col)
                                .filter(|c| !gb_cols.contains(c)),
                        )
                        .collect()
                });
                // combine and dedup
                let dedup_gb_cols: Vec<_> = gb_cols
                    .into_iter()
                    .filter(|gbc| !param_cols.contains(gbc))
                    .collect();
                let gb_and_param_cols: Vec<Column> = dedup_gb_cols
                    .into_iter()
                    .chain(param_cols.into_iter())
                    .map(Column::from)
                    .collect();

                let mut have_parent_cols = HashSet::new();
                // we cannot have duplicate columns at the data-flow level, as it confuses our
                // migration analysis code.
                let gb_and_param_cols = gb_and_param_cols
                    .into_iter()
                    .filter_map(|mut c| {
                        let pn = parent_node.borrow();
                        let pc = pn.columns().iter().position(|pc| *pc == c);
                        if pc.is_none() {
                            Some(c)
                        } else if !have_parent_cols.contains(&pc) {
                            have_parent_cols.insert(pc);
                            let pc = pn.columns()[pc.unwrap()].clone();
                            if pc.name != c.name || pc.table != c.table {
                                // remember the alias with the parent column
                                c.aliases.push(pc);
                            }
                            Some(c)
                        } else {
                            // we already have this column, so eliminate duplicate
                            None
                        }
                    })
                    .collect();

                (parent_node, gb_and_param_cols)
            } else {
                let proj_cols_from_target_table = over_cols
                    .flat_map(|col| &qg.relations[col.table.as_ref().unwrap()].columns)
                    .map(Column::from)
                    .collect::<Vec<_>>();

                let (group_cols, parent_node) = if proj_cols_from_target_table.is_empty() {
                    // slightly messy hack: if there are no group columns and the
                    // table on which we compute has no projected columns in the
                    // output, we make one up a group column by adding an extra
                    // projection node
                    let proj_name = format!("{}_prj_hlpr", name);
                    let fn_cols: Vec<_> =
                        Expression::Call(computed_col.function.as_deref().unwrap().clone())
                            .referred_columns()
                            .map(|c| Column::from(c.into_owned()))
                            .collect();
                    // TODO(grfn) this double-collect is really gross- make_projection_helper takes
                    // a Vec<&mir::Column> but we have a Vec<&nom_sql::Column> and there's no way to
                    // make the former from the latter without doing some upsetting allocations
                    let fn_cols = fn_cols.iter().collect();
                    let proj =
                        mir_converter.make_projection_helper(&proj_name, parent_node, fn_cols);

                    func_nodes.push(proj.clone());
                    node_count += 1;

                    let bogo_group_col = Column::new(None, "grp");
                    (vec![bogo_group_col], proj)
                } else {
                    (proj_cols_from_target_table, parent_node)
                };

                (parent_node, group_cols)
            };

            let nodes: Vec<MirNodeRef> = mir_converter.make_aggregate_node(
                name,
                &Column::from(computed_col),
                group_cols.iter().collect(),
                parent_node.clone(),
            );

            *prev_node = Some(nodes.last().unwrap().clone());
            node_count += nodes.len();
            func_nodes.extend(nodes);
        }
    }

    Ok(func_nodes)
}
