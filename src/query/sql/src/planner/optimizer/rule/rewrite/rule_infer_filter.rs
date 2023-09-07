// Copyright 2021 Datafuse Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::Arc;

use common_exception::Result;
use common_expression::Scalar;
use std::collections::HashMap;

use crate::IndexType;
use crate::binder::split_conjunctions;
use crate::plans::ComparisonOp;
use crate::optimizer::property::constraint::remove_trivial_type_cast;
use crate::optimizer::rule::Rule;
use crate::optimizer::rule::TransformResult;
use crate::optimizer::RuleID;
use crate::optimizer::SExpr;
use crate::plans::ConstantExpr;
use crate::plans::Filter;
use crate::plans::FunctionCall;
use crate::ScalarExpr::BoundColumnRef;
use crate::plans::PatternPlan;
use crate::plans::RelOp;
use crate::plans::ScalarExpr;

pub struct RuleInferFilter {
    id: RuleID,
    patterns: Vec<SExpr>,
}



impl RuleInferFilter {
    pub fn new() -> Self {
        Self {
            id: RuleID::InferFilter,
            // Filter
            //  \
            //   *
            patterns: vec![SExpr::create_unary(
                Arc::new(
                    PatternPlan {
                        plan_type: RelOp::Filter,
                    }
                    .into(),
                ),
                Arc::new(SExpr::create_leaf(Arc::new(
                    PatternPlan {
                        plan_type: RelOp::Pattern,
                    }
                    .into(),
                ))),
            )],
        }
    }
}

pub struct ColumnPredicate {
    op: ComparisonOp,
    scalar: ConstantExpr,
}

pub struct PredicateSet {
    column_predicates: HashMap<ScalarExpr, Vec<ColumnPredicate>>
}

impl PredicateSet {
    fn merge_predicate(&mut self, index: ScalarExpr, right: ColumnPredicate) {
        match self.column_predicates.get_mut(&index) {
            Some(predicates) => {
                for (idx, left) in predicates.iter().enumerate() {
                    Self::prune(left, &right)
                }
                predicates.push(right);
            }
            None => {
                self.column_predicates.insert(index, vec![right]);
            }
        }
    }

    fn prune(left: &ColumnPredicate, right: &ColumnPredicate) {
        match left.op {
            ComparisonOp::Equal => {
                match right.op {
                    ComparisonOp::Equal => {

                    }
                    ComparisonOp::NotEqual => {

                    }
                    ComparisonOp::LT => {

                    }
                    ComparisonOp::LTE => {

                    }
                    ComparisonOp::GT => {

                    }
                    ComparisonOp::GTE => {

                    }
                }
            }
            ComparisonOp::NotEqual => {
                match right.op {
                    ComparisonOp::Equal => {

                    }
                    ComparisonOp::NotEqual => {

                    }
                    ComparisonOp::LT => {

                    }
                    ComparisonOp::LTE => {

                    }
                    ComparisonOp::GT => {

                    }
                    ComparisonOp::GTE => {

                    }
                }
            }
            ComparisonOp::LT => {
                match right.op {
                    ComparisonOp::Equal => {

                    }
                    ComparisonOp::NotEqual => {

                    }
                    ComparisonOp::LT => {

                    }
                    ComparisonOp::LTE => {

                    }
                    ComparisonOp::GT => {
                        if left.scalar < right.scalar {
                            dbg!("less", &left.scalar, &right.scalar);
                        } else {
                            dbg!("greater", &left.scalar, &right.scalar);
                        }
                    }
                    ComparisonOp::GTE => {

                    }
                }
            }
            ComparisonOp::GT => {
                match right.op {
                    ComparisonOp::Equal => {

                    }
                    ComparisonOp::NotEqual => {

                    }
                    ComparisonOp::LT => {

                    }
                    ComparisonOp::LTE => {

                    }
                    ComparisonOp::GT => {
                        if left.scalar < right.scalar {
                            dbg!("less", &left.scalar, &right.scalar);
                        } else {
                            dbg!("greater", &left.scalar, &right.scalar);
                        }
                    }
                    ComparisonOp::GTE => {

                    }
                }
            }
            _ => ()
        }
    }
}

impl Rule for RuleInferFilter {
    fn id(&self) -> RuleID {
        self.id
    }

    fn apply(&self, s_expr: &SExpr, state: &mut TransformResult) -> Result<()> {
        let filter: Filter = s_expr.plan().clone().try_into()?;
        let mut predicates = filter.predicates;
        let mut predicate_set = PredicateSet {
            column_predicates: HashMap::new()
        };

        for predicate in predicates.iter_mut() {
            match predicate {
                ScalarExpr::FunctionCall(func) => {
                    if let Some(_) = ComparisonOp::try_from_func_name(&func.func_name) {
                        (func.arguments[0], func.arguments[1]) =
                            remove_trivial_type_cast(func.arguments[0].clone(), func.arguments[1].clone());
                    }
                }
                _ => (),
            }
        }

        let mut remaining_predicates = Vec::new();
        for predicate in predicates.into_iter() {
            dbg!(&predicate, &predicate.column_index());
            match &predicate {
                ScalarExpr::FunctionCall(func) => {
                    if let Some(op) = ComparisonOp::try_from_func_name(&func.func_name) {
                        let left_index = func.arguments[0].column_index();
                        let right_index = func.arguments[1].column_index();
                        match (left_index, right_index) {
                            (Some(_), Some(_)) => {
                                remaining_predicates.push(predicate);
                            },
                            (Some(left_index), None) => {
                                predicate_set.merge_predicate(func.arguments[0].clone(), ColumnPredicate { op, scalar: func.arguments[1].constant().unwrap() });
                                remaining_predicates.push(predicate);
                            },
                            (None, Some(right_index)) => {
                                predicate_set.merge_predicate(func.arguments[1].clone(), ColumnPredicate { op, scalar: func.arguments[0].constant().unwrap() });
                                remaining_predicates.push(predicate);
                            },
                            (None, None) => {
                                remaining_predicates.push(predicate);
                            },
                        }
                        // if let (
                        //     ScalarExpr::BoundColumnRef(left_col),
                        //     ScalarExpr::BoundColumnRef(right_col),
                        // ) = (&func.arguments[0], &func.arguments[1])
                        // {
                        //     left_col.column.index != right_col.column.index
                        //         || left_col.column.data_type.is_nullable()
                        // } else {
                        //     true
                        // }
                    } else {
                        remaining_predicates.push(predicate);
                    }
                }
                // if func.func_name == "eq" => {
                    // if let (
                    //     ScalarExpr::BoundColumnRef(left_col),
                    //     ScalarExpr::BoundColumnRef(right_col),
                    // ) = (&func.arguments[0], &func.arguments[1])
                    // {
                    //     left_col.column.index != right_col.column.index
                    //         || left_col.column.data_type.is_nullable()
                    // } else {
                    //     true
                    // }
                // }
                _ => remaining_predicates.push(predicate),
            }
        }

        // let mut rewritten_predicates = Vec::with_capacity(predicates.len());
        // let mut rewritten = false;
        // for predicate in predicates.iter() {
        //     let predicate_scalar = predicate_scalar(predicate);
        //     let (rewritten_predicate_scalar, has_rewritten) =
        //         rewrite_predicate_ors(predicate_scalar);
        //     if has_rewritten {
        //         rewritten = true;
        //     }
        //     rewritten_predicates.push(normalize_predicate_scalar(rewritten_predicate_scalar));
        // }
        // let mut split_predicates: Vec<ScalarExpr> = Vec::with_capacity(rewritten_predicates.len());
        // for predicate in rewritten_predicates.iter() {
        //     split_predicates.extend_from_slice(&split_conjunctions(predicate));
        // }
        // if rewritten {
        //     state.add_result(SExpr::create_unary(
        //         Arc::new(
        //             Filter {
        //                 predicates: split_predicates,
        //                 is_having: filter.is_having,
        //             }
        //             .into(),
        //         ),
        //         Arc::new(s_expr.child(0)?.clone()),
        //     ));
        // }
        Ok(())
    }

    fn patterns(&self) -> &Vec<SExpr> {
        &self.patterns
    }
}
