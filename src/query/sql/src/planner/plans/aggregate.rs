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

use common_catalog::table_context::TableContext;
use common_exception::ErrorCode;
use common_exception::Result;

use crate::optimizer::ColumnSet;
use crate::optimizer::Distribution;
use crate::optimizer::PhysicalProperty;
use crate::optimizer::RelExpr;
use crate::optimizer::RelationalProperty;
use crate::optimizer::RequiredProperty;
use crate::optimizer::StatInfo;
use crate::optimizer::Statistics;
use crate::plans::Operator;
use crate::plans::RelOp;
use crate::plans::ScalarItem;
use crate::IndexType;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Copy)]
pub enum AggregateMode {
    Partial,
    Final,

    // TODO(leiysky): this mode is only used for preventing recursion of
    // RuleSplitAggregate, find a better way.
    Initial,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Aggregate {
    pub mode: AggregateMode,
    // group by scalar expressions, such as: group by col1, col2;
    pub group_items: Vec<ScalarItem>,
    // aggregate scalar expressions, such as: sum(col1), count(*);
    pub aggregate_functions: Vec<ScalarItem>,
    // True if the plan is generated from distinct, else the plan is a normal aggregate;
    pub from_distinct: bool,
    pub limit: Option<usize>,
    /// The index of the virtual column `_grouping_id`. It's valid only if `grouping_sets` is not empty.
    pub grouping_id_index: IndexType,
    /// The grouping sets, each grouping set is a list of `group_items` indices.
    pub grouping_sets: Vec<Vec<IndexType>>,
}

impl Aggregate {
    pub fn used_columns(&self) -> Result<ColumnSet> {
        let mut used_columns = ColumnSet::new();
        for group_item in self.group_items.iter() {
            used_columns.insert(group_item.index);
            used_columns.extend(group_item.scalar.used_columns())
        }
        for agg in self.aggregate_functions.iter() {
            used_columns.insert(agg.index);
            used_columns.extend(agg.scalar.used_columns())
        }
        Ok(used_columns)
    }

    pub fn group_columns(&self) -> Result<ColumnSet> {
        let mut col_set = ColumnSet::new();
        for group_item in self.group_items.iter() {
            col_set.insert(group_item.index);
            col_set.extend(group_item.scalar.used_columns())
        }
        Ok(col_set)
    }
}

impl Operator for Aggregate {
    fn rel_op(&self) -> RelOp {
        RelOp::Aggregate
    }

    fn derive_physical_prop(&self, rel_expr: &RelExpr) -> Result<PhysicalProperty> {
        rel_expr.derive_physical_prop_child(0)
    }

    fn compute_required_prop_child(
        &self,
        ctx: Arc<dyn TableContext>,
        rel_expr: &RelExpr,
        _child_index: usize,
        required: &RequiredProperty,
    ) -> Result<RequiredProperty> {
        let mut required = required.clone();
        let child_physical_prop = rel_expr.derive_physical_prop_child(0)?;

        if child_physical_prop.distribution == Distribution::Serial {
            return Ok(required);
        }

        match self.mode {
            AggregateMode::Partial => {
                if self.group_items.is_empty() {
                    // Scalar aggregation
                    required.distribution = Distribution::Any;
                } else {
                    let settings = ctx.get_settings();

                    // Group aggregation, enforce `Hash` distribution
                    required.distribution = match settings.get_group_by_shuffle_mode()?.as_str() {
                        "before_partial" => Ok(Distribution::Hash(
                            self.group_items
                                .iter()
                                .map(|item| item.scalar.clone())
                                .collect(),
                        )),
                        "before_merge" => {
                            Ok(Distribution::Hash(vec![self.group_items[0].scalar.clone()]))
                        }
                        value => Err(ErrorCode::Internal(format!(
                            "Bad settings value group_by_shuffle_mode = {:?}",
                            value
                        ))),
                    }?;
                }
            }

            AggregateMode::Final => {
                if self.group_items.is_empty() {
                    // Scalar aggregation
                    required.distribution = Distribution::Serial;
                } else {
                    // The distribution should have been derived by partial aggregation
                    required.distribution = Distribution::Any;
                }
            }

            AggregateMode::Initial => unreachable!(),
        }
        Ok(required)
    }

    fn derive_relational_prop(&self, rel_expr: &RelExpr) -> Result<Arc<RelationalProperty>> {
        let input_prop = rel_expr.derive_relational_prop_child(0)?;

        // Derive output columns
        let mut output_columns = ColumnSet::new();
        for group_item in self.group_items.iter() {
            output_columns.insert(group_item.index);
        }
        for agg in self.aggregate_functions.iter() {
            output_columns.insert(agg.index);
        }

        // Derive outer columns
        let outer_columns = input_prop
            .outer_columns
            .difference(&output_columns)
            .cloned()
            .collect();

        // Derive used columns
        let mut used_columns = self.used_columns()?;
        used_columns.extend(input_prop.used_columns.clone());

        Ok(Arc::new(RelationalProperty {
            output_columns,
            outer_columns,
            used_columns,
        }))
    }

    fn derive_cardinality(&self, rel_expr: &RelExpr) -> Result<Arc<StatInfo>> {
        if self.mode == AggregateMode::Final {
            return rel_expr.derive_cardinality_child(0);
        }
        let stat_info = rel_expr.derive_cardinality_child(0)?;
        let (cardinality, mut statistics) = (stat_info.cardinality, stat_info.statistics.clone());
        let cardinality = if self.group_items.is_empty() {
            // Scalar aggregation
            1.0
        } else if self
            .group_items
            .iter()
            .any(|item| statistics.column_stats.get(&item.index).is_none())
        {
            cardinality
        } else {
            let mut res: f64 = 0.0;
            let mut max_ndv_idx = 0;
            let mut ndvs = Vec::with_capacity(self.group_items.len());
            for (idx, item) in self.group_items.iter().enumerate() {
                let item_stat: &crate::optimizer::ColumnStat = statistics.column_stats.get(&item.index).unwrap();
                if item_stat.ndv > res {
                    res = item_stat.ndv;
                    max_ndv_idx = idx;
                }
                ndvs.push(item_stat.ndv);
            }
            let mut group_num = cardinality / res;
            for (idx, ndv) in ndvs.iter().enumerate() {
                if idx == max_ndv_idx {
                    continue;
                }
                let group_ndv = ndv / cardinality * group_num;
                if group_ndv > 1.0 {
                    res *= group_ndv;
                    group_num /= group_ndv;
                }
            }
            for item in self.group_items.iter() {
                let item_stat = statistics.column_stats.get_mut(&item.index).unwrap();
                if let Some(histogram) = &mut item_stat.histogram {
                    for bucket in histogram.buckets.iter_mut() {
                        bucket.aggregate_values();
                    }
                }
            }
            // To avoid res is very large
            f64::min(res, cardinality)
        };

        let precise_cardinality = if self.group_items.is_empty() {
            Some(1)
        } else {
            None
        };
        Ok(Arc::new(StatInfo {
            cardinality,
            statistics: Statistics {
                precise_cardinality,
                column_stats: statistics.column_stats,
            },
        }))
    }
}
