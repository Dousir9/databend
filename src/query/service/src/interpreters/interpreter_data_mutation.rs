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

use databend_common_base::base::ProgressValues;
use databend_common_catalog::lock::LockTableOption;
use databend_common_catalog::plan::Partitions;
use databend_common_catalog::plan::PartitionsShuffleKind;
use databend_common_catalog::table::TableExt;
use databend_common_exception::ErrorCode;
use databend_common_exception::Result;
use databend_common_expression::types::UInt32Type;
use databend_common_expression::DataBlock;
use databend_common_expression::DataSchemaRef;
use databend_common_expression::FromData;
use databend_common_expression::SendableDataBlockStream;
use databend_common_sql::binder::DataMutationInputType;
use databend_common_sql::executor::physical_plans::create_push_down_filters;
use databend_common_sql::executor::physical_plans::MutationKind;
use databend_common_sql::executor::DataMutationBuildInfo;
use databend_common_sql::executor::PhysicalPlan;
use databend_common_sql::executor::PhysicalPlanBuilder;
use databend_common_sql::optimizer::SExpr;
use databend_common_sql::plans;
use databend_common_sql::plans::DataMutation;
use databend_common_storages_factory::Table;
use databend_common_storages_fuse::operations::TruncateMode;
use databend_common_storages_fuse::FuseTable;
use databend_common_storages_fuse::TableContext;
use databend_storages_common_table_meta::meta::TableSnapshot;

use crate::interpreters::common::check_deduplicate_label;
use crate::interpreters::common::dml_build_update_stream_req;
use crate::interpreters::HookOperator;
use crate::interpreters::Interpreter;
use crate::pipelines::PipelineBuildResult;
use crate::schedulers::build_query_pipeline_without_render_result_set;
use crate::sessions::QueryContext;
use crate::stream::DataBlockStream;

pub struct DataMutationInterpreter {
    ctx: Arc<QueryContext>,
    s_expr: SExpr,
    schema: DataSchemaRef,
}

impl DataMutationInterpreter {
    pub fn try_create(
        ctx: Arc<QueryContext>,
        s_expr: SExpr,
        schema: DataSchemaRef,
    ) -> Result<DataMutationInterpreter> {
        Ok(DataMutationInterpreter {
            ctx,
            s_expr,
            schema,
        })
    }
}

#[async_trait::async_trait]
impl Interpreter for DataMutationInterpreter {
    fn name(&self) -> &str {
        "DataMutationInterpreter"
    }

    fn is_ddl(&self) -> bool {
        false
    }

    #[async_backtrace::framed]
    async fn execute2(&self) -> Result<PipelineBuildResult> {
        if check_deduplicate_label(self.ctx.clone()).await? {
            return Ok(PipelineBuildResult::create());
        }

        let data_mutation: DataMutation = self.s_expr.plan().clone().try_into()?;

        let table = self
            .ctx
            .get_table(
                &data_mutation.catalog_name,
                &data_mutation.database_name,
                &data_mutation.table_name,
            )
            .await?;

        // Check if the table supports DataMutation.
        table.check_mutable()?;
        let fuse_table = table.as_any().downcast_ref::<FuseTable>().ok_or_else(|| {
            ErrorCode::Unimplemented(format!(
                "table {}, engine type {}, does not support {}",
                table.name(),
                table.get_table_info().engine(),
                data_mutation.input_type,
            ))
        })?;

        let table_snapshot = fuse_table.read_table_snapshot().await?;
        if let Some(build_res) = self
            .fast_mutation(&data_mutation, fuse_table, &table_snapshot)
            .await?
        {
            return Ok(build_res);
        }

        // Prepare DataMutationBuildInfo for PhysicalPlanBuilder to build DataMutation physical plan.
        let table_info = fuse_table.get_table_info().clone();
        let update_stream_meta =
            dml_build_update_stream_req(self.ctx.clone(), &data_mutation.metadata).await?;
        let partitions = self
            .mutation_source_partions(&data_mutation, fuse_table, table_snapshot.clone())
            .await?;
        let data_mutation_build_info = DataMutationBuildInfo {
            table_info,
            table_snapshot,
            update_stream_meta,
            partitions,
        };

        // Build physical plan.
        let physical_plan = self
            .build_physical_plan(&data_mutation, Some(data_mutation_build_info))
            .await?;

        // Build pipeline.
        let mut build_res =
            build_query_pipeline_without_render_result_set(&self.ctx, &physical_plan).await?;

        // Execute hook.
        self.execute_hook(&data_mutation, &mut build_res).await;

        build_res
            .main_pipeline
            .add_lock_guard(data_mutation.lock_guard);

        Ok(build_res)
    }

    fn inject_result(&self) -> Result<SendableDataBlockStream> {
        let blocks = self.get_data_mutation_table_result()?;
        Ok(Box::pin(DataBlockStream::create(None, blocks)))
    }
}

impl DataMutationInterpreter {
    pub async fn execute_hook(
        &self,
        data_mutation: &databend_common_sql::plans::DataMutation,
        build_res: &mut PipelineBuildResult,
    ) {
        let hook_lock_opt = if data_mutation.lock_guard.is_some() {
            LockTableOption::NoLock
        } else {
            LockTableOption::LockNoRetry
        };

        let mutation_kind = match data_mutation.input_type {
            DataMutationInputType::Update => MutationKind::Update,
            DataMutationInputType::Delete => MutationKind::Delete,
            DataMutationInputType::Merge => MutationKind::MergeInto,
        };

        let hook_operator = HookOperator::create(
            self.ctx.clone(),
            data_mutation.catalog_name.clone(),
            data_mutation.database_name.clone(),
            data_mutation.table_name.clone(),
            mutation_kind,
            hook_lock_opt,
        );
        match data_mutation.input_type {
            DataMutationInputType::Update | DataMutationInputType::Delete => {
                hook_operator
                    .execute_refresh(&mut build_res.main_pipeline)
                    .await
            }
            DataMutationInputType::Merge => {
                hook_operator.execute(&mut build_res.main_pipeline).await
            }
        };
    }

    pub async fn build_physical_plan(
        &self,
        data_mutation: &DataMutation,
        data_mutation_build_info: Option<DataMutationBuildInfo>,
    ) -> Result<PhysicalPlan> {
        let data_mutation_build_info =
            if let Some(data_mutation_build_info) = data_mutation_build_info {
                data_mutation_build_info
            } else {
                let table = self
                    .ctx
                    .get_table(
                        &data_mutation.catalog_name,
                        &data_mutation.database_name,
                        &data_mutation.table_name,
                    )
                    .await?;

                // Check if the table supports DataMutation.
                table.check_mutable()?;
                let fuse_table = table.as_any().downcast_ref::<FuseTable>().ok_or_else(|| {
                    ErrorCode::Unimplemented(format!(
                        "table {}, engine type {}, does not support {}",
                        table.name(),
                        table.get_table_info().engine(),
                        data_mutation.input_type,
                    ))
                })?;

                // Prepare DataMutationBuildInfo for PhysicalPlanBuilder to build DataMutation physical plan.
                let table_info = fuse_table.get_table_info().clone();
                let table_snapshot = fuse_table.read_table_snapshot().await?;
                let update_stream_meta =
                    dml_build_update_stream_req(self.ctx.clone(), &data_mutation.metadata).await?;
                let partitions = self
                    .mutation_source_partions(data_mutation, fuse_table, table_snapshot.clone())
                    .await?;
                DataMutationBuildInfo {
                    table_info,
                    table_snapshot,
                    update_stream_meta,
                    partitions,
                }
            };

        // Build physical plan.
        let mut builder =
            PhysicalPlanBuilder::new(data_mutation.metadata.clone(), self.ctx.clone(), false);
        builder.set_data_mutation_build_info(data_mutation_build_info);
        builder
            .build(&self.s_expr, *data_mutation.required_columns.clone())
            .await
    }

    fn get_data_mutation_table_result(&self) -> Result<Vec<DataBlock>> {
        let binding = self.ctx.get_merge_status();
        let status = binding.read();
        let mut columns = Vec::new();
        for field in self.schema.as_ref().fields() {
            match field.name().as_str() {
                plans::INSERT_NAME => {
                    columns.push(UInt32Type::from_data(vec![status.insert_rows as u32]))
                }
                plans::UPDATE_NAME => {
                    columns.push(UInt32Type::from_data(vec![status.update_rows as u32]))
                }
                plans::DELETE_NAME => {
                    columns.push(UInt32Type::from_data(vec![status.deleted_rows as u32]))
                }
                _ => unreachable!(),
            }
        }
        Ok(vec![DataBlock::new_from_columns(columns)])
    }

    async fn fast_mutation(
        &self,
        data_mutation: &DataMutation,
        fuse_table: &FuseTable,
        snapshot: &Option<Arc<TableSnapshot>>,
    ) -> Result<Option<PipelineBuildResult>> {
        if data_mutation.input_type == DataMutationInputType::Merge {
            return Ok(None);
        }

        let mut build_res = PipelineBuildResult::create();

        // Check if table is empty.
        let Some(snapshot) = snapshot else {
            // No snapshot, no mutation.
            return Ok(Some(build_res));
        };
        if snapshot.summary.row_count == 0 {
            // Empty snapshot, no mutation.
            return Ok(Some(build_res));
        }

        if data_mutation.input_type == DataMutationInputType::Delete {
            if data_mutation.truncate_table {
                let progress_values = ProgressValues {
                    rows: snapshot.summary.row_count as usize,
                    bytes: snapshot.summary.uncompressed_byte_size as usize,
                };
                self.ctx.get_write_progress().incr(&progress_values);
                // deleting the whole table... just a truncate
                fuse_table
                    .do_truncate(
                        self.ctx.clone(),
                        &mut build_res.main_pipeline,
                        TruncateMode::Delete,
                    )
                    .await?;
                Ok(Some(build_res))
            } else {
                Ok(None)
            }
        } else {
            Ok(None)
        }
    }

    async fn mutation_source_partions(
        &self,
        data_mutation: &DataMutation,
        fuse_table: &FuseTable,
        table_snapshot: Option<Arc<TableSnapshot>>,
    ) -> Result<Option<Partitions>> {
        if data_mutation.mutation_source {
            let Some(table_snapshot) = table_snapshot else {
                return Ok(Some(Partitions::create(PartitionsShuffleKind::Mod, vec![])));
            };
            let (filters, filter_used_columns) =
                if let Some(filter) = &data_mutation.mutation_filter {
                    (
                        Some(create_push_down_filters(filter)?),
                        filter.used_columns().into_iter().collect(),
                    )
                } else {
                    (None, vec![])
                };
            let (is_lazy, is_delete) = if data_mutation.input_type == DataMutationInputType::Delete
            {
                let cluster = self.ctx.get_cluster();
                let is_lazy =
                    !cluster.is_empty() && table_snapshot.segments.len() >= cluster.nodes.len();
                (is_lazy, true)
            } else {
                (false, false)
            };
            Ok(Some(
                fuse_table
                    .mutation_read_partitions(
                        self.ctx.clone(),
                        table_snapshot,
                        filter_used_columns,
                        filters,
                        is_lazy,
                        is_delete,
                    )
                    .await?,
            ))
        } else {
            Ok(None)
        }
    }
}
