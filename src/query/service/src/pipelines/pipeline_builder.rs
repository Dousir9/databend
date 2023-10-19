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

use std::collections::HashMap;
use std::convert::TryFrom;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;

use async_channel::Receiver;
use common_ast::parser::parse_comma_separated_exprs;
use common_ast::parser::tokenize_sql;
use common_base::base::tokio::sync::Barrier;
use common_base::base::tokio::sync::Semaphore;
use common_base::runtime::Runtime;
use common_catalog::plan::Projection;
use common_catalog::table::AppendMode;
use common_exception::ErrorCode;
use common_exception::Result;
use common_expression::type_check::check_function;
use common_expression::types::DataType;
use common_expression::types::NumberDataType;
use common_expression::with_hash_method;
use common_expression::with_mappedhash_method;
use common_expression::with_number_mapped_type;
use common_expression::ColumnBuilder;
use common_expression::DataBlock;
use common_expression::DataSchema;
use common_expression::DataSchemaRef;
use common_expression::FunctionContext;
use common_expression::HashMethodKind;
use common_expression::Scalar;
use common_expression::SortColumnDescription;
use common_formats::FastFieldDecoderValues;
use common_formats::FastValuesDecodeFallback;
use common_formats::FastValuesDecoder;
use common_functions::aggregates::AggregateFunctionFactory;
use common_functions::aggregates::AggregateFunctionRef;
use common_functions::BUILTIN_FUNCTIONS;
use common_pipeline_core::pipe::Pipe;
use common_pipeline_core::pipe::PipeItem;
use common_pipeline_core::processors::port::InputPort;
use common_pipeline_core::processors::port::OutputPort;
use common_pipeline_core::processors::processor::ProcessorPtr;
use common_pipeline_core::processors::Processor;
use common_pipeline_core::query_spill_prefix;
use common_pipeline_sinks::EmptySink;
use common_pipeline_sinks::Sinker;
use common_pipeline_sinks::UnionReceiveSink;
use common_pipeline_sources::AsyncSource;
use common_pipeline_sources::AsyncSourcer;
use common_pipeline_sources::EmptySource;
use common_pipeline_sources::OneBlockSource;
use common_pipeline_transforms::processors::profile_wrapper::ProcessorProfileWrapper;
use common_pipeline_transforms::processors::profile_wrapper::ProfileStub;
use common_pipeline_transforms::processors::profile_wrapper::TransformProfileWrapper;
use common_pipeline_transforms::processors::transforms::build_full_sort_pipeline;
use common_pipeline_transforms::processors::transforms::create_dummy_item;
use common_pipeline_transforms::processors::transforms::Transformer;
use common_profile::SharedProcessorProfiles;
use common_settings::Settings;
use common_sql::evaluator::BlockOperator;
use common_sql::evaluator::CompoundBlockOperator;
use common_sql::executor::AggregateExpand;
use common_sql::executor::AggregateFinal;
use common_sql::executor::AggregateFunctionDesc;
use common_sql::executor::AggregatePartial;
use common_sql::executor::AsyncSourcerPlan;
use common_sql::executor::CommitSink;
use common_sql::executor::CompactSource;
use common_sql::executor::ConstantTableScan;
use common_sql::executor::CopyIntoTablePhysicalPlan;
use common_sql::executor::CopyIntoTableSource;
use common_sql::executor::CteScan;
use common_sql::executor::Deduplicate;
use common_sql::executor::DeleteSource;
use common_sql::executor::DistributedInsertSelect;
use common_sql::executor::EvalScalar;
use common_sql::executor::ExchangeSink;
use common_sql::executor::ExchangeSource;
use common_sql::executor::Filter;
use common_sql::executor::HashJoin;
use common_sql::executor::Lambda;
use common_sql::executor::Limit;
use common_sql::executor::MaterializedCte;
use common_sql::executor::MergeInto;
use common_sql::executor::MergeIntoSource;
use common_sql::executor::PhysicalPlan;
use common_sql::executor::Project;
use common_sql::executor::ProjectSet;
use common_sql::executor::RangeJoin;
use common_sql::executor::ReplaceInto;
use common_sql::executor::RowFetch;
use common_sql::executor::RuntimeFilterSource;
use common_sql::executor::SelectCtx;
use common_sql::executor::Sort;
use common_sql::executor::TableScan;
use common_sql::executor::UnionAll;
use common_sql::executor::Window;
use common_sql::BindContext;
use common_sql::ColumnBinding;
use common_sql::IndexType;
use common_sql::Metadata;
use common_sql::MetadataRef;
use common_sql::NameResolutionContext;
use common_storage::DataOperator;
use common_storages_factory::Table;
use common_storages_fuse::operations::build_row_fetcher_pipeline;
use common_storages_fuse::operations::common::TransformSerializeSegment;
use common_storages_fuse::operations::merge_into::MatchedSplitProcessor;
use common_storages_fuse::operations::merge_into::MergeIntoNotMatchedProcessor;
use common_storages_fuse::operations::merge_into::MergeIntoSplitProcessor;
use common_storages_fuse::operations::replace_into::BroadcastProcessor;
use common_storages_fuse::operations::replace_into::ReplaceIntoProcessor;
use common_storages_fuse::operations::replace_into::UnbranchedReplaceIntoProcessor;
use common_storages_fuse::operations::FillInternalColumnProcessor;
use common_storages_fuse::operations::MutationBlockPruningContext;
use common_storages_fuse::operations::TransformSerializeBlock;
use common_storages_fuse::FuseLazyPartInfo;
use common_storages_fuse::FuseTable;
use common_storages_fuse::SegmentLocation;
use common_storages_stage::StageTable;
use log::info;
use parking_lot::RwLock;

use super::processors::transforms::FrameBound;
use super::processors::transforms::TransformAddComputedColumns;
use super::processors::transforms::WindowFunctionInfo;
use super::processors::TransformExpandGroupingSets;
use super::processors::TransformResortAddOnWithoutSourceSchema;
use crate::api::DefaultExchangeInjector;
use crate::api::ExchangeInjector;
use crate::pipelines::builders::build_append_data_pipeline;
use crate::pipelines::builders::build_fill_missing_columns_pipeline;
use crate::pipelines::processors::transforms::build_partition_bucket;
use crate::pipelines::processors::transforms::hash_join::BuildSpillCoordinator;
use crate::pipelines::processors::transforms::hash_join::BuildSpillState;
use crate::pipelines::processors::transforms::hash_join::HashJoinBuildState;
use crate::pipelines::processors::transforms::hash_join::HashJoinProbeState;
use crate::pipelines::processors::transforms::hash_join::ProbeSpillState;
use crate::pipelines::processors::transforms::hash_join::TransformHashJoinBuild;
use crate::pipelines::processors::transforms::hash_join::TransformHashJoinProbe;
use crate::pipelines::processors::transforms::range_join::TransformRangeJoinLeft;
use crate::pipelines::processors::transforms::range_join::TransformRangeJoinRight;
use crate::pipelines::processors::transforms::AggregateInjector;
use crate::pipelines::processors::transforms::FinalSingleStateAggregator;
use crate::pipelines::processors::transforms::HashJoinDesc;
use crate::pipelines::processors::transforms::MaterializedCteSink;
use crate::pipelines::processors::transforms::MaterializedCteSource;
use crate::pipelines::processors::transforms::MaterializedCteState;
use crate::pipelines::processors::transforms::PartialSingleStateAggregator;
use crate::pipelines::processors::transforms::RangeJoinState;
use crate::pipelines::processors::transforms::RuntimeFilterState;
use crate::pipelines::processors::transforms::TransformAggregateSpillWriter;
use crate::pipelines::processors::transforms::TransformGroupBySpillWriter;
use crate::pipelines::processors::transforms::TransformMergeBlock;
use crate::pipelines::processors::transforms::TransformPartialAggregate;
use crate::pipelines::processors::transforms::TransformPartialGroupBy;
use crate::pipelines::processors::transforms::TransformWindow;
use crate::pipelines::processors::AggregatorParams;
use crate::pipelines::processors::HashJoinState;
use crate::pipelines::processors::SinkRuntimeFilterSource;
use crate::pipelines::processors::TransformCastSchema;
use crate::pipelines::processors::TransformLimit;
use crate::pipelines::processors::TransformRuntimeFilter;
use crate::pipelines::Pipeline;
use crate::pipelines::PipelineBuildResult;
use crate::sessions::QueryContext;
use crate::sessions::TableContext;
use crate::sql::executor::MutationKind;

pub struct PipelineBuilder {
    ctx: Arc<QueryContext>,

    main_pipeline: Pipeline,
    pub pipelines: Vec<Pipeline>,
    func_ctx: FunctionContext,
    settings: Arc<Settings>,

    // Used in runtime filter source
    pub join_state: Option<Arc<HashJoinBuildState>>,
    // record the index of join build side pipeline in `pipelines`
    pub index: Option<usize>,

    // Cte -> state, each cte has it's own state
    pub cte_state: HashMap<IndexType, Arc<MaterializedCteState>>,

    enable_profiling: bool,
    proc_profs: SharedProcessorProfiles,
    exchange_injector: Arc<dyn ExchangeInjector>,
}

impl PipelineBuilder {
    pub fn create(
        func_ctx: FunctionContext,
        settings: Arc<Settings>,
        ctx: Arc<QueryContext>,
        enable_profiling: bool,
        prof_span_set: SharedProcessorProfiles,
    ) -> PipelineBuilder {
        PipelineBuilder {
            enable_profiling,
            ctx,
            func_ctx,
            settings,
            pipelines: vec![],
            join_state: None,
            main_pipeline: Pipeline::create(),
            proc_profs: prof_span_set,
            exchange_injector: DefaultExchangeInjector::create(),
            index: None,
            cte_state: HashMap::new(),
        }
    }

    pub fn finalize(mut self, plan: &PhysicalPlan) -> Result<PipelineBuildResult> {
        self.build_pipeline(plan)?;

        for source_pipeline in &self.pipelines {
            if !source_pipeline.is_complete_pipeline()? {
                return Err(ErrorCode::Internal(
                    "Source pipeline must be complete pipeline.",
                ));
            }
        }

        Ok(PipelineBuildResult {
            main_pipeline: self.main_pipeline,
            sources_pipelines: self.pipelines,
            prof_span_set: self.proc_profs,
            exchange_injector: self.exchange_injector,
        })
    }

    fn build_pipeline(&mut self, plan: &PhysicalPlan) -> Result<()> {
        match plan {
            PhysicalPlan::TableScan(scan) => self.build_table_scan(scan),
            PhysicalPlan::CteScan(scan) => self.build_cte_scan(scan),
            PhysicalPlan::ConstantTableScan(scan) => self.build_constant_table_scan(scan),
            PhysicalPlan::Filter(filter) => self.build_filter(filter),
            PhysicalPlan::Project(project) => self.build_project(project),
            PhysicalPlan::EvalScalar(eval_scalar) => self.build_eval_scalar(eval_scalar),
            PhysicalPlan::AggregateExpand(aggregate) => self.build_aggregate_expand(aggregate),
            PhysicalPlan::AggregatePartial(aggregate) => self.build_aggregate_partial(aggregate),
            PhysicalPlan::AggregateFinal(aggregate) => self.build_aggregate_final(aggregate),
            PhysicalPlan::Window(window) => self.build_window(window),
            PhysicalPlan::Sort(sort) => self.build_sort(sort),
            PhysicalPlan::Limit(limit) => self.build_limit(limit),
            PhysicalPlan::RowFetch(row_fetch) => self.build_row_fetch(row_fetch),
            PhysicalPlan::HashJoin(join) => self.build_join(join),
            PhysicalPlan::ExchangeSink(sink) => self.build_exchange_sink(sink),
            PhysicalPlan::ExchangeSource(source) => self.build_exchange_source(source),
            PhysicalPlan::UnionAll(union_all) => self.build_union_all(union_all),
            PhysicalPlan::DistributedInsertSelect(insert_select) => {
                self.build_distributed_insert_select(insert_select)
            }
            PhysicalPlan::ProjectSet(project_set) => self.build_project_set(project_set),
            PhysicalPlan::Lambda(lambda) => self.build_lambda(lambda),
            PhysicalPlan::Exchange(_) => Err(ErrorCode::Internal(
                "Invalid physical plan with PhysicalPlan::Exchange",
            )),
            PhysicalPlan::RuntimeFilterSource(runtime_filter_source) => {
                self.build_runtime_filter_source(runtime_filter_source)
            }
            PhysicalPlan::DeleteSource(delete) => self.build_delete_source(delete),
            PhysicalPlan::CompactSource(compact) => self.build_compact_source(compact),
            PhysicalPlan::CommitSink(plan) => self.build_commit_sink(plan),
            PhysicalPlan::RangeJoin(range_join) => self.build_range_join(range_join),
            PhysicalPlan::MaterializedCte(materialized_cte) => {
                self.build_materialized_cte(materialized_cte)
            }
            PhysicalPlan::CopyIntoTable(copy) => self.build_copy_into_table(copy),
            PhysicalPlan::AsyncSourcer(async_sourcer) => self.build_async_sourcer(async_sourcer),
            PhysicalPlan::Deduplicate(deduplicate) => self.build_deduplicate(deduplicate),
            PhysicalPlan::ReplaceInto(replace) => self.build_replace_into(replace),
            PhysicalPlan::MergeInto(merge_into) => self.build_merge_into(merge_into),
            PhysicalPlan::MergeIntoSource(merge_into_source) => {
                self.build_merge_into_source(merge_into_source)
            }
        }
    }

    fn check_schema_cast(
        select_schema: Arc<DataSchema>,
        output_schema: Arc<DataSchema>,
    ) -> Result<bool> {
        // check if cast needed
        let cast_needed = select_schema != output_schema;
        Ok(cast_needed)
    }

    fn build_merge_into_source(&mut self, merge_into_source: &MergeIntoSource) -> Result<()> {
        let MergeIntoSource { input, row_id_idx } = merge_into_source;

        self.build_pipeline(input)?;
        // merge into's parallism depends on the join probe number.
        let mut items = Vec::with_capacity(self.main_pipeline.output_len());
        let output_len = self.main_pipeline.output_len();
        for _ in 0..output_len {
            let merge_into_split_processor = MergeIntoSplitProcessor::create(*row_id_idx, false)?;
            items.push(merge_into_split_processor.into_pipe_item());
        }

        self.main_pipeline
            .add_pipe(Pipe::create(output_len, output_len * 2, items));

        Ok(())
    }

    fn build_deduplicate(&mut self, deduplicate: &Deduplicate) -> Result<()> {
        let Deduplicate {
            input,
            on_conflicts,
            bloom_filter_column_indexes,
            table_is_empty,
            table_info,
            catalog_info,
            select_ctx,
            table_level_range_index,
            table_schema,
            need_insert,
            delete_when,
        } = deduplicate;

        let tbl = self
            .ctx
            .build_table_by_table_info(catalog_info, table_info, None)?;
        let table = FuseTable::try_from_table(tbl.as_ref())?;
        self.build_pipeline(input)?;
        let mut delete_column_idx = 0;
        let mut opt_modified_schema = None;
        if let Some(SelectCtx {
            select_column_bindings,
            select_schema,
        }) = select_ctx
        {
            PipelineBuilder::render_result_set(
                &self.func_ctx,
                input.output_schema()?,
                select_column_bindings,
                &mut self.main_pipeline,
                false,
            )?;

            let mut target_schema: DataSchema = table_schema.clone().into();
            if let Some((_, delete_column)) = delete_when {
                delete_column_idx = select_schema.index_of(delete_column.as_str())?;
                let delete_column = select_schema.field(delete_column_idx).clone();
                target_schema
                    .fields
                    .insert(delete_column_idx, delete_column);
                opt_modified_schema = Some(Arc::new(target_schema.clone()));
            }
            let target_schema = Arc::new(target_schema.clone());
            if target_schema.fields().len() != select_schema.fields().len() {
                return Err(ErrorCode::BadArguments(
                    "The number of columns in the target table is different from the number of columns in the SELECT clause",
                ));
            }
            if Self::check_schema_cast(select_schema.clone(), target_schema.clone())? {
                self.main_pipeline.add_transform(
                    |transform_input_port, transform_output_port| {
                        TransformCastSchema::try_create(
                            transform_input_port,
                            transform_output_port,
                            select_schema.clone(),
                            target_schema.clone(),
                            self.func_ctx.clone(),
                        )
                    },
                )?;
            }
        }

        build_fill_missing_columns_pipeline(
            self.ctx.clone(),
            &mut self.main_pipeline,
            tbl.clone(),
            Arc::new(table_schema.clone().into()),
        )?;

        let _ = table.cluster_gen_for_append(
            self.ctx.clone(),
            &mut self.main_pipeline,
            table.get_block_thresholds(),
            opt_modified_schema,
        )?;
        // 1. resize input to 1, since the UpsertTransform need to de-duplicate inputs "globally"
        self.main_pipeline.try_resize(1)?;

        // 2. connect with ReplaceIntoProcessor

        //                      ┌──────────────────────┐
        //                      │                      ├──┐
        // ┌─────────────┐      │                      ├──┘
        // │ UpsertSource├─────►│ ReplaceIntoProcessor │
        // └─────────────┘      │                      ├──┐
        //                      │                      ├──┘
        //                      └──────────────────────┘
        // NOTE: here the pipe items of last pipe are arranged in the following order
        // (0) -> output_port_append_data
        // (1) -> output_port_merge_into_action
        //    the "downstream" is supposed to be connected with a processor which can process MergeIntoOperations
        //    in our case, it is the broadcast processor
        let delete_when = if let Some((remote_expr, delete_column)) = delete_when {
            Some((
                remote_expr.as_expr(&BUILTIN_FUNCTIONS),
                delete_column.clone(),
            ))
        } else {
            None
        };
        let cluster_keys = table.cluster_keys(self.ctx.clone());
        if *need_insert {
            let replace_into_processor = ReplaceIntoProcessor::create(
                self.ctx.clone(),
                on_conflicts.clone(),
                cluster_keys,
                bloom_filter_column_indexes.clone(),
                table_schema.as_ref(),
                *table_is_empty,
                table_level_range_index.clone(),
                delete_when.map(|(expr, _)| (expr, delete_column_idx)),
            )?;
            self.main_pipeline
                .add_pipe(replace_into_processor.into_pipe());
        } else {
            let replace_into_processor = UnbranchedReplaceIntoProcessor::create(
                self.ctx.as_ref(),
                on_conflicts.clone(),
                cluster_keys,
                bloom_filter_column_indexes.clone(),
                table_schema.as_ref(),
                *table_is_empty,
                table_level_range_index.clone(),
                delete_when.map(|_| delete_column_idx),
            )?;
            self.main_pipeline
                .add_pipe(replace_into_processor.into_pipe());
        }
        Ok(())
    }

    fn build_merge_into(&mut self, merge_into: &MergeInto) -> Result<()> {
        let MergeInto {
            input,
            table_info,
            catalog_info,
            unmatched,
            matched,
            field_index_of_input_schema,
            row_id_idx,
            segments,
        } = merge_into;

        self.build_pipeline(input)?;
        let tbl = self
            .ctx
            .build_table_by_table_info(catalog_info, table_info, None)?;

        let table = FuseTable::try_from_table(tbl.as_ref())?;
        let block_thresholds = table.get_block_thresholds();

        let cluster_stats_gen =
            table.get_cluster_stats_gen(self.ctx.clone(), 0, block_thresholds, None)?;

        // this TransformSerializeBlock is just used to get block_builder
        let block_builder = TransformSerializeBlock::try_create(
            self.ctx.clone(),
            InputPort::create(),
            OutputPort::create(),
            table,
            cluster_stats_gen.clone(),
        )?
        .get_block_builder();

        let serialize_segment_transform = TransformSerializeSegment::new(
            self.ctx.clone(),
            InputPort::create(),
            OutputPort::create(),
            table,
            block_thresholds,
        );
        let get_output_len = |pipe_items: &Vec<PipeItem>| -> usize {
            let mut output_len = 0;
            for item in pipe_items.iter() {
                output_len += item.outputs_port.len();
            }
            output_len
        };
        // Handle matched and unmatched data separately.

        //                                                                                 +-----------------------------+-+
        //                                    +-----------------------+     Matched        |                             +-+
        //                                    |                       +---+--------------->|    MatchedSplitProcessor    |
        //                                    |                       |   |                |                             +-+
        // +----------------------+           |                       +---+                +-----------------------------+-+
        // |   MergeIntoSource    +---------->|MergeIntoSplitProcessor|
        // +----------------------+           |                       +---+                +-----------------------------+
        //                                    |                       |   | NotMatched     |                             +-+
        //                                    |                       +---+--------------->| MergeIntoNotMatchedProcessor| |
        //                                    +-----------------------+                    |                             +-+
        //                                                                                 +-----------------------------+
        // Note: here the output_port of MatchedSplitProcessor are arranged in the following order
        // (0) -> output_port_row_id
        // (1) -> output_port_updated

        // Outputs from MatchedSplitProcessor's output_port_updated and MergeIntoNotMatchedProcessor's output_port are merged and processed uniformly by the subsequent ResizeProcessor

        // receive matched data and not matched data parallelly.
        let mut pipe_items = Vec::with_capacity(self.main_pipeline.output_len());
        for _ in (0..self.main_pipeline.output_len()).step_by(2) {
            let matched_split_processor = MatchedSplitProcessor::create(
                self.ctx.clone(),
                *row_id_idx,
                matched.clone(),
                field_index_of_input_schema.clone(),
                input.output_schema()?,
                Arc::new(DataSchema::from(tbl.schema())),
            )?;

            let merge_into_not_matched_processor = MergeIntoNotMatchedProcessor::create(
                unmatched.clone(),
                input.output_schema()?,
                self.func_ctx.clone(),
            )?;

            pipe_items.push(matched_split_processor.into_pipe_item());
            pipe_items.push(merge_into_not_matched_processor.into_pipe_item());
        }
        self.main_pipeline.add_pipe(Pipe::create(
            self.main_pipeline.output_len(),
            get_output_len(&pipe_items),
            pipe_items.clone(),
        ));

        // row_id port0_1
        // matched update data port0_2
        // not macthed insert data port0_3
        // row_id port1_1
        // matched update data port1_2
        // not macthed insert data port1_3
        // ......
        assert_eq!(self.main_pipeline.output_len() % 3, 0);

        // merge rowid and serialize_blocks
        let mut ranges = Vec::with_capacity(self.main_pipeline.output_len() / 3 * 2);
        for idx in (0..self.main_pipeline.output_len()).step_by(3) {
            ranges.push(vec![idx]);
            ranges.push(vec![idx + 1, idx + 2]);
        }
        self.main_pipeline.resize_partial_one(ranges.clone())?;
        assert_eq!(self.main_pipeline.output_len() % 2, 0);

        // shuffle outputs and resize row_id
        // ----------------------------------------------------------------------
        // row_id port0_1               row_id port0_1
        // data port0_2                 row_id port1_1              row_id port
        // row_id port1_1   ======>     ......           ======>
        // data port1_2                 data port0_2                data port0
        // ......                       data port1_2                data port1
        // ......                       .....                       .....
        // ----------------------------------------------------------------------
        let mut rules = Vec::with_capacity(self.main_pipeline.output_len());
        for idx in 0..(self.main_pipeline.output_len() / 2) {
            rules.push(idx);
            rules.push(idx + self.main_pipeline.output_len() / 2);
        }
        self.main_pipeline.reorder_inputs(rules);
        // resize row_id
        ranges.clear();
        let mut vec = Vec::with_capacity(self.main_pipeline.output_len() / 2);
        for idx in 0..(self.main_pipeline.output_len() / 2) {
            vec.push(idx);
        }
        ranges.push(vec.clone());
        for idx in 0..(self.main_pipeline.output_len() / 2) {
            ranges.push(vec![idx + self.main_pipeline.output_len() / 2]);
        }
        self.main_pipeline.resize_partial_one(ranges.clone())?;

        // fill default columns
        let table_default_schema = &table.schema().remove_computed_fields();
        let mut builder = self.main_pipeline.add_transform_with_specified_len(
            |transform_input_port, transform_output_port| {
                TransformResortAddOnWithoutSourceSchema::try_create(
                    self.ctx.clone(),
                    transform_input_port,
                    transform_output_port,
                    Arc::new(DataSchema::from(table_default_schema)),
                    tbl.clone(),
                )
            },
            self.main_pipeline.output_len() - 1,
        )?;
        builder.add_items_prepend(vec![create_dummy_item()]);
        self.main_pipeline.add_pipe(builder.finalize());

        // fill computed columns
        let table_computed_schema = &table.schema().remove_virtual_computed_fields();
        let default_schema: DataSchemaRef = Arc::new(table_default_schema.into());
        let computed_schema: DataSchemaRef = Arc::new(table_computed_schema.into());
        if default_schema != computed_schema {
            builder = self.main_pipeline.add_transform_with_specified_len(
                |transform_input_port, transform_output_port| {
                    TransformAddComputedColumns::try_create(
                        self.ctx.clone(),
                        transform_input_port,
                        transform_output_port,
                        default_schema.clone(),
                        computed_schema.clone(),
                    )
                },
                self.main_pipeline.output_len() - 1,
            )?;
            builder.add_items_prepend(vec![create_dummy_item()]);
            self.main_pipeline.add_pipe(builder.finalize());
        }

        let max_threads = self.settings.get_max_threads()?;
        let io_request_semaphore = Arc::new(Semaphore::new(max_threads as usize));

        // after filling default columns, we need to add cluster‘s blocksort if it's a cluster table
        let output_lens = self.main_pipeline.output_len();
        table.cluster_gen_for_append_with_specified_last_len(
            self.ctx.clone(),
            &mut self.main_pipeline,
            block_thresholds,
            output_lens - 1,
        )?;

        pipe_items.clear();
        pipe_items.push(table.rowid_aggregate_mutator(
            self.ctx.clone(),
            block_builder,
            io_request_semaphore,
            segments.clone(),
        )?);

        for _ in 0..self.main_pipeline.output_len() - 1 {
            let serialize_block_transform = TransformSerializeBlock::try_create(
                self.ctx.clone(),
                InputPort::create(),
                OutputPort::create(),
                table,
                cluster_stats_gen.clone(),
            )?;
            pipe_items.push(serialize_block_transform.into_pipe_item());
        }

        self.main_pipeline.add_pipe(Pipe::create(
            self.main_pipeline.output_len(),
            get_output_len(&pipe_items),
            pipe_items,
        ));

        // resize block ports
        // aggregate_mutator port               aggregate_mutator port
        // serialize_block port0     ======>
        // serialize_block port1                serialize_block port
        // .......
        ranges.clear();
        ranges.push(vec![0]);
        vec.clear();
        for idx in 0..self.main_pipeline.output_len() - 1 {
            vec.push(idx + 1);
        }
        ranges.push(vec);
        self.main_pipeline.resize_partial_one(ranges)?;

        let pipe_items = vec![
            create_dummy_item(),
            serialize_segment_transform.into_pipe_item(),
        ];

        self.main_pipeline.add_pipe(Pipe::create(
            self.main_pipeline.output_len(),
            get_output_len(&pipe_items),
            pipe_items,
        ));

        Ok(())
    }

    fn build_replace_into(&mut self, replace: &ReplaceInto) -> Result<()> {
        let ReplaceInto {
            input,
            block_thresholds,
            table_info,
            on_conflicts,
            bloom_filter_column_indexes,
            catalog_info,
            segments,
            block_slots,
            need_insert,
        } = replace;
        let max_threads = self.settings.get_max_threads()?;
        let segment_partition_num = std::cmp::min(segments.len(), max_threads as usize);
        let table = self
            .ctx
            .build_table_by_table_info(catalog_info, table_info, None)?;
        let table = FuseTable::try_from_table(table.as_ref())?;
        let cluster_stats_gen =
            table.get_cluster_stats_gen(self.ctx.clone(), 0, *block_thresholds, None)?;
        self.build_pipeline(input)?;
        // connect to broadcast processor and append transform
        let serialize_block_transform = TransformSerializeBlock::try_create(
            self.ctx.clone(),
            InputPort::create(),
            OutputPort::create(),
            table,
            cluster_stats_gen,
        )?;
        let block_builder = serialize_block_transform.get_block_builder();

        let serialize_segment_transform = TransformSerializeSegment::new(
            self.ctx.clone(),
            InputPort::create(),
            OutputPort::create(),
            table,
            *block_thresholds,
        );
        if !*need_insert {
            if segment_partition_num == 0 {
                return Ok(());
            }
            let broadcast_processor = BroadcastProcessor::new(segment_partition_num);
            self.main_pipeline
                .add_pipe(Pipe::create(1, segment_partition_num, vec![
                    broadcast_processor.into_pipe_item(),
                ]));
            let max_threads = self.settings.get_max_threads()?;
            let io_request_semaphore = Arc::new(Semaphore::new(max_threads as usize));

            let merge_into_operation_aggregators = table.merge_into_mutators(
                self.ctx.clone(),
                segment_partition_num,
                block_builder,
                on_conflicts.clone(),
                bloom_filter_column_indexes.clone(),
                segments,
                block_slots.clone(),
                io_request_semaphore,
            )?;
            self.main_pipeline.add_pipe(Pipe::create(
                segment_partition_num,
                segment_partition_num,
                merge_into_operation_aggregators,
            ));
            return Ok(());
        }

        if segment_partition_num == 0 {
            let dummy_item = create_dummy_item();
            //                      ┌──────────────────────┐            ┌──────────────────┐
            //                      │                      ├──┬────────►│  SerializeBlock  │
            // ┌─────────────┐      │                      ├──┘         └──────────────────┘
            // │ UpsertSource├─────►│ ReplaceIntoProcessor │
            // └─────────────┘      │                      ├──┐         ┌──────────────────┐
            //                      │                      ├──┴────────►│  DummyTransform  │
            //                      └──────────────────────┘            └──────────────────┘
            // wrap them into pipeline, order matters!
            self.main_pipeline.add_pipe(Pipe::create(2, 2, vec![
                serialize_block_transform.into_pipe_item(),
                dummy_item,
            ]));
        } else {
            //                      ┌──────────────────────┐            ┌──────────────────┐
            //                      │                      ├──┬────────►│ SerializeBlock   │
            // ┌─────────────┐      │                      ├──┘         └──────────────────┘
            // │ UpsertSource├─────►│ ReplaceIntoProcessor │
            // └─────────────┘      │                      ├──┐         ┌──────────────────┐
            //                      │                      ├──┴────────►│BroadcastProcessor│
            //                      └──────────────────────┘            └──────────────────┘
            let broadcast_processor = BroadcastProcessor::new(segment_partition_num);
            // wrap them into pipeline, order matters!
            self.main_pipeline
                .add_pipe(Pipe::create(2, segment_partition_num + 1, vec![
                    serialize_block_transform.into_pipe_item(),
                    broadcast_processor.into_pipe_item(),
                ]));
        };

        // 4. connect with MergeIntoOperationAggregators
        if segment_partition_num == 0 {
            let dummy_item = create_dummy_item();
            self.main_pipeline.add_pipe(Pipe::create(2, 2, vec![
                serialize_segment_transform.into_pipe_item(),
                dummy_item,
            ]));
        } else {
            //      ┌──────────────────┐               ┌────────────────┐
            // ────►│  SerializeBlock  ├──────────────►│SerializeSegment│
            //      └──────────────────┘               └────────────────┘
            //
            //      ┌───────────────────┐              ┌──────────────────────┐
            // ────►│                   ├──┬──────────►│MergeIntoOperationAggr│
            //      │                   ├──┘           └──────────────────────┘
            //      │ BroadcastProcessor│
            //      │                   ├──┐           ┌──────────────────────┐
            //      │                   ├──┴──────────►│MergeIntoOperationAggr│
            //      │                   │              └──────────────────────┘
            //      │                   ├──┐
            //      │                   ├──┴──────────►┌──────────────────────┐
            //      └───────────────────┘              │MergeIntoOperationAggr│
            //                                         └──────────────────────┘

            let item_size = segment_partition_num + 1;
            let mut pipe_items = Vec::with_capacity(item_size);
            // setup the dummy transform
            pipe_items.push(serialize_segment_transform.into_pipe_item());

            let max_threads = self.settings.get_max_threads()?;
            let io_request_semaphore = Arc::new(Semaphore::new(max_threads as usize));

            // setup the merge into operation aggregators
            let mut merge_into_operation_aggregators = table.merge_into_mutators(
                self.ctx.clone(),
                segment_partition_num,
                block_builder,
                on_conflicts.clone(),
                bloom_filter_column_indexes.clone(),
                segments,
                block_slots.clone(),
                io_request_semaphore,
            )?;
            assert_eq!(
                segment_partition_num,
                merge_into_operation_aggregators.len()
            );
            pipe_items.append(&mut merge_into_operation_aggregators);

            // extend the pipeline
            assert_eq!(self.main_pipeline.output_len(), item_size);
            assert_eq!(pipe_items.len(), item_size);
            self.main_pipeline
                .add_pipe(Pipe::create(item_size, item_size, pipe_items));
        }
        Ok(())
    }

    fn build_async_sourcer(&mut self, async_sourcer: &AsyncSourcerPlan) -> Result<()> {
        self.main_pipeline.add_source(
            |output| {
                let name_resolution_ctx = NameResolutionContext::try_from(self.settings.as_ref())?;
                let inner = ValueSource::new(
                    async_sourcer.value_data.clone(),
                    self.ctx.clone(),
                    name_resolution_ctx,
                    async_sourcer.schema.clone(),
                    async_sourcer.start,
                );
                AsyncSourcer::create(self.ctx.clone(), output, inner)
            },
            1,
        )?;
        Ok(())
    }

    fn build_copy_into_table(&mut self, copy: &CopyIntoTablePhysicalPlan) -> Result<()> {
        let to_table =
            self.ctx
                .build_table_by_table_info(&copy.catalog_info, &copy.table_info, None)?;
        let source_schema = match &copy.source {
            CopyIntoTableSource::Query(input) => {
                self.build_pipeline(&input.plan)?;
                Self::render_result_set(
                    &self.func_ctx,
                    input.plan.output_schema()?,
                    &input.result_columns,
                    &mut self.main_pipeline,
                    input.ignore_result,
                )?;
                input.query_source_schema.clone()
            }
            CopyIntoTableSource::Stage(source) => {
                let stage_table = StageTable::try_create(copy.stage_table_info.clone())?;
                stage_table.set_block_thresholds(to_table.get_block_thresholds());
                stage_table.read_data(self.ctx.clone(), source, &mut self.main_pipeline)?;
                copy.required_source_schema.clone()
            }
        };
        build_append_data_pipeline(
            self.ctx.clone(),
            &mut self.main_pipeline,
            copy,
            source_schema,
            to_table,
        )?;
        Ok(())
    }

    fn build_compact_source(&mut self, compact_block: &CompactSource) -> Result<()> {
        let table = self.ctx.build_table_by_table_info(
            &compact_block.catalog_info,
            &compact_block.table_info,
            None,
        )?;
        let table = FuseTable::try_from_table(table.as_ref())?;

        if compact_block.parts.is_empty() {
            return self.main_pipeline.add_source(EmptySource::create, 1);
        }

        table.build_compact_source(
            self.ctx.clone(),
            compact_block.parts.clone(),
            compact_block.column_ids.clone(),
            &mut self.main_pipeline,
        )
    }

    /// The flow of Pipeline is as follows:
    ///
    /// +---------------+      +-----------------------+
    /// |MutationSource1| ---> |SerializeDataTransform1|
    /// +---------------+      +-----------------------+
    /// |     ...       | ---> |          ...          |
    /// +---------------+      +-----------------------+
    /// |MutationSourceN| ---> |SerializeDataTransformN|
    /// +---------------+      +-----------------------+
    fn build_delete_source(&mut self, delete: &DeleteSource) -> Result<()> {
        let table =
            self.ctx
                .build_table_by_table_info(&delete.catalog_info, &delete.table_info, None)?;
        let table = FuseTable::try_from_table(table.as_ref())?;

        if delete.parts.is_empty() {
            return self.main_pipeline.add_source(EmptySource::create, 1);
        }

        if delete.parts.is_lazy {
            let ctx = self.ctx.clone();
            let projection = Projection::Columns(delete.col_indices.clone());
            let filters = delete.filters.clone();
            let table_clone = table.clone();
            let mut segment_locations = Vec::with_capacity(delete.parts.partitions.len());
            for part in &delete.parts.partitions {
                // Safe to downcast because we know the the partition is lazy
                let part: &FuseLazyPartInfo = part.as_any().downcast_ref().unwrap();
                segment_locations.push(SegmentLocation {
                    segment_idx: part.segment_index,
                    location: part.segment_location.clone(),
                    snapshot_loc: None,
                });
            }
            let prune_ctx = MutationBlockPruningContext {
                segment_locations,
                block_count: None,
            };
            self.main_pipeline.set_on_init(move || {
                let ctx_clone = ctx.clone();
                let (partitions, info) =
                    Runtime::with_worker_threads(2, None)?.block_on(async move {
                        table_clone
                            .do_mutation_block_pruning(
                                ctx_clone,
                                Some(filters),
                                projection,
                                prune_ctx,
                                true,
                                true,
                            )
                            .await
                    })?;
                info!(
                    "delete pruning done, number of whole block deletion detected in pruning phase: {}",
                    info.num_whole_block_mutation
                );
                ctx.set_partitions(partitions)?;
                Ok(())
            });
        } else {
            self.ctx.set_partitions(delete.parts.clone())?;
        }
        table.add_deletion_source(
            self.ctx.clone(),
            &delete.filters.filter,
            delete.col_indices.clone(),
            delete.query_row_id_col,
            &mut self.main_pipeline,
        )?;
        let cluster_stats_gen =
            table.get_cluster_stats_gen(self.ctx.clone(), 0, table.get_block_thresholds(), None)?;
        self.main_pipeline.add_transform(|input, output| {
            let proc = TransformSerializeBlock::try_create(
                self.ctx.clone(),
                input,
                output,
                table,
                cluster_stats_gen.clone(),
            )?;
            proc.into_processor()
        })?;
        let ctx: Arc<dyn TableContext> = self.ctx.clone();
        if delete.parts.is_lazy {
            table.chain_mutation_aggregator(
                &ctx,
                &mut self.main_pipeline,
                delete.snapshot.clone(),
                MutationKind::Delete,
            )?;
        }
        Ok(())
    }

    fn build_commit_sink(&mut self, plan: &CommitSink) -> Result<()> {
        self.build_pipeline(&plan.input)?;
        let table =
            self.ctx
                .build_table_by_table_info(&plan.catalog_info, &plan.table_info, None)?;
        let table = FuseTable::try_from_table(table.as_ref())?;
        let ctx: Arc<dyn TableContext> = self.ctx.clone();

        table.chain_mutation_pipes(
            &ctx,
            &mut self.main_pipeline,
            plan.snapshot.clone(),
            plan.mutation_kind,
            plan.merge_meta,
        )?;
        Ok(())
    }

    fn build_range_join(&mut self, range_join: &RangeJoin) -> Result<()> {
        let state = Arc::new(RangeJoinState::new(self.ctx.clone(), range_join));
        self.expand_right_side_pipeline(range_join, state.clone())?;
        self.build_left_side(range_join, state)?;
        if self.enable_profiling {
            self.main_pipeline.add_transform(|input, output| {
                Ok(ProcessorPtr::create(Transformer::create(
                    input,
                    output,
                    ProfileStub::new(range_join.plan_id, self.proc_profs.clone())
                        .accumulate_output_rows()
                        .accumulate_output_bytes(),
                )))
            })?;
        }
        Ok(())
    }

    fn build_left_side(
        &mut self,
        range_join: &RangeJoin,
        state: Arc<RangeJoinState>,
    ) -> Result<()> {
        self.build_pipeline(&range_join.left)?;
        let max_threads = self.settings.get_max_threads()? as usize;
        self.main_pipeline.try_resize(max_threads)?;
        self.main_pipeline.add_transform(|input, output| {
            let transform = TransformRangeJoinLeft::create(input, output, state.clone());
            if self.enable_profiling {
                Ok(ProcessorPtr::create(ProcessorProfileWrapper::create(
                    transform,
                    range_join.plan_id,
                    self.proc_profs.clone(),
                )))
            } else {
                Ok(ProcessorPtr::create(transform))
            }
        })?;
        Ok(())
    }

    fn expand_right_side_pipeline(
        &mut self,
        range_join: &RangeJoin,
        state: Arc<RangeJoinState>,
    ) -> Result<()> {
        let right_side_context = QueryContext::create_from(self.ctx.clone());
        let mut right_side_builder = PipelineBuilder::create(
            self.func_ctx.clone(),
            self.settings.clone(),
            right_side_context,
            self.enable_profiling,
            self.proc_profs.clone(),
        );
        right_side_builder.cte_state = self.cte_state.clone();
        let mut right_res = right_side_builder.finalize(&range_join.right)?;
        right_res.main_pipeline.add_sink(|input| {
            let transform = Sinker::<TransformRangeJoinRight>::create(
                input,
                TransformRangeJoinRight::create(state.clone()),
            );
            if self.enable_profiling {
                Ok(ProcessorPtr::create(ProcessorProfileWrapper::create(
                    transform,
                    range_join.plan_id,
                    self.proc_profs.clone(),
                )))
            } else {
                Ok(ProcessorPtr::create(transform))
            }
        })?;
        self.pipelines.push(right_res.main_pipeline);
        self.pipelines
            .extend(right_res.sources_pipelines.into_iter());
        Ok(())
    }

    fn build_join(&mut self, join: &HashJoin) -> Result<()> {
        let state = self.build_join_state(join)?;
        self.expand_build_side_pipeline(&join.build, join, state.clone())?;
        self.build_join_probe(join, state)
    }

    fn build_join_state(&mut self, join: &HashJoin) -> Result<Arc<HashJoinState>> {
        HashJoinState::try_create(
            self.ctx.clone(),
            join.build.output_schema()?,
            &join.build_projections,
            HashJoinDesc::create(join)?,
            &join.probe_to_build,
        )
    }

    fn expand_build_side_pipeline(
        &mut self,
        build: &PhysicalPlan,
        hash_join_plan: &HashJoin,
        join_state: Arc<HashJoinState>,
    ) -> Result<()> {
        let build_side_context = QueryContext::create_from(self.ctx.clone());
        let mut build_side_builder = PipelineBuilder::create(
            self.func_ctx.clone(),
            self.settings.clone(),
            build_side_context,
            self.enable_profiling,
            self.proc_profs.clone(),
        );
        build_side_builder.cte_state = self.cte_state.clone();
        let mut build_res = build_side_builder.finalize(build)?;

        assert!(build_res.main_pipeline.is_pulling_pipeline()?);
        let output_len = build_res.main_pipeline.output_len();
        let spill_coordinator = BuildSpillCoordinator::create(output_len);
        let barrier = Barrier::new(output_len);
        let restore_barrier = Barrier::new(output_len);
        let build_state = HashJoinBuildState::try_create(
            self.ctx.clone(),
            self.func_ctx.clone(),
            &hash_join_plan.build_keys,
            &hash_join_plan.build_projections,
            join_state,
            barrier,
            restore_barrier,
        )?;

        let create_sink_processor = |input| {
            let spill_state = if self.settings.get_join_spilling_threshold()? != 0 {
                Some(Box::new(BuildSpillState::create(
                    self.ctx.clone(),
                    spill_coordinator.clone(),
                    build_state.clone(),
                )))
            } else {
                None
            };
            let transform =
                TransformHashJoinBuild::try_create(input, build_state.clone(), spill_state)?;

            if self.enable_profiling {
                Ok(ProcessorPtr::create(ProcessorProfileWrapper::create(
                    transform,
                    hash_join_plan.plan_id,
                    self.proc_profs.clone(),
                )))
            } else {
                Ok(ProcessorPtr::create(transform))
            }
        };
        if hash_join_plan.contain_runtime_filter {
            build_res.main_pipeline.duplicate(false)?;
            self.join_state = Some(build_state);
            self.index = Some(self.pipelines.len());
        } else {
            build_res.main_pipeline.add_sink(create_sink_processor)?;
        }

        self.pipelines.push(build_res.main_pipeline);
        self.pipelines
            .extend(build_res.sources_pipelines.into_iter());
        Ok(())
    }

    pub fn render_result_set(
        func_ctx: &FunctionContext,
        input_schema: DataSchemaRef,
        result_columns: &[ColumnBinding],
        pipeline: &mut Pipeline,
        ignore_result: bool,
    ) -> Result<()> {
        if ignore_result {
            return pipeline.add_sink(|input| Ok(ProcessorPtr::create(EmptySink::create(input))));
        }

        let mut projections = Vec::with_capacity(result_columns.len());

        for column_binding in result_columns {
            let index = column_binding.index;
            projections.push(input_schema.index_of(index.to_string().as_str())?);
        }
        let num_input_columns = input_schema.num_fields();
        pipeline.add_transform(|input, output| {
            Ok(ProcessorPtr::create(CompoundBlockOperator::create(
                input,
                output,
                num_input_columns,
                func_ctx.clone(),
                vec![BlockOperator::Project {
                    projection: projections.clone(),
                }],
            )))
        })?;

        Ok(())
    }

    fn build_table_scan(&mut self, scan: &TableScan) -> Result<()> {
        let table = self.ctx.build_table_from_source_plan(&scan.source)?;
        self.ctx.set_partitions(scan.source.parts.clone())?;
        table.read_data(self.ctx.clone(), &scan.source, &mut self.main_pipeline)?;

        if self.enable_profiling {
            self.main_pipeline.add_transform(|input, output| {
                // shared timer between `on_start` and `on_finish`
                let start_timer = Arc::new(Mutex::new(Instant::now()));
                let finish_timer = Arc::new(Mutex::new(Instant::now()));
                Ok(ProcessorPtr::create(Transformer::create(
                    input,
                    output,
                    ProfileStub::new(scan.plan_id, self.proc_profs.clone())
                        .on_start(move |v| {
                            *start_timer.lock().unwrap() = Instant::now();
                            *v
                        })
                        .on_finish(move |prof| {
                            let elapsed = finish_timer.lock().unwrap().elapsed();
                            let mut prof = *prof;
                            prof.wait_time = elapsed;
                            prof
                        })
                        .accumulate_output_bytes()
                        .accumulate_output_rows(),
                )))
            })?;
        }

        // Fill internal columns if needed.
        if let Some(internal_columns) = &scan.internal_column {
            if table.support_row_id_column() {
                self.main_pipeline.add_transform(|input, output| {
                    Ok(ProcessorPtr::create(Box::new(
                        FillInternalColumnProcessor::create(
                            internal_columns.clone(),
                            input,
                            output,
                        ),
                    )))
                })?;
            } else {
                return Err(ErrorCode::TableEngineNotSupported(format!(
                    "Table engine `{}` does not support virtual column _row_id",
                    table.engine()
                )));
            }
        }

        let schema = scan.source.schema();
        let mut projection = scan
            .name_mapping
            .keys()
            .map(|name| schema.index_of(name.as_str()))
            .collect::<Result<Vec<usize>>>()?;
        projection.sort();

        // if projection is sequential, no need to add projection
        if projection != (0..schema.fields().len()).collect::<Vec<usize>>() {
            let ops = vec![BlockOperator::Project { projection }];
            let num_input_columns = schema.num_fields();
            self.main_pipeline.add_transform(|input, output| {
                Ok(ProcessorPtr::create(CompoundBlockOperator::create(
                    input,
                    output,
                    num_input_columns,
                    self.func_ctx.clone(),
                    ops.clone(),
                )))
            })?;
        }

        Ok(())
    }

    fn build_cte_scan(&mut self, cte_scan: &CteScan) -> Result<()> {
        let max_threads = self.settings.get_max_threads()?;
        self.main_pipeline.add_source(
            |output| {
                MaterializedCteSource::create(
                    self.ctx.clone(),
                    output,
                    cte_scan.cte_idx,
                    self.cte_state.get(&cte_scan.cte_idx.0).unwrap().clone(),
                    cte_scan.offsets.clone(),
                )
            },
            max_threads as usize,
        )
    }

    fn build_constant_table_scan(&mut self, scan: &ConstantTableScan) -> Result<()> {
        self.main_pipeline.add_source(
            |output| {
                let block = if !scan.values.is_empty() {
                    DataBlock::new_from_columns(scan.values.clone())
                } else {
                    DataBlock::new(vec![], scan.num_rows)
                };
                OneBlockSource::create(output, block)
            },
            1,
        )
    }

    fn build_filter(&mut self, filter: &Filter) -> Result<()> {
        self.build_pipeline(&filter.input)?;

        let predicate = filter
            .predicates
            .iter()
            .map(|expr| expr.as_expr(&BUILTIN_FUNCTIONS))
            .try_reduce(|lhs, rhs| {
                check_function(None, "and_filters", &[], &[lhs, rhs], &BUILTIN_FUNCTIONS)
            })
            .transpose()
            .unwrap_or_else(|| {
                Err(ErrorCode::Internal(
                    "Invalid empty predicate list".to_string(),
                ))
            })?;

        let num_input_columns = filter.input.output_schema()?.num_fields();
        self.main_pipeline.add_transform(|input, output| {
            let transform = CompoundBlockOperator::new(
                vec![BlockOperator::Filter {
                    projections: filter.projections.clone(),
                    expr: predicate.clone(),
                }],
                self.func_ctx.clone(),
                num_input_columns,
            );

            if self.enable_profiling {
                Ok(ProcessorPtr::create(TransformProfileWrapper::create(
                    transform,
                    input,
                    output,
                    filter.plan_id,
                    self.proc_profs.clone(),
                )))
            } else {
                Ok(ProcessorPtr::create(Transformer::create(
                    input, output, transform,
                )))
            }
        })?;

        Ok(())
    }

    fn build_project(&mut self, project: &Project) -> Result<()> {
        self.build_pipeline(&project.input)?;
        let num_input_columns = project.input.output_schema()?.num_fields();
        self.main_pipeline.add_transform(|input, output| {
            Ok(ProcessorPtr::create(CompoundBlockOperator::create(
                input,
                output,
                num_input_columns,
                self.func_ctx.clone(),
                vec![BlockOperator::Project {
                    projection: project.projections.clone(),
                }],
            )))
        })
    }

    fn build_eval_scalar(&mut self, eval_scalar: &EvalScalar) -> Result<()> {
        self.build_pipeline(&eval_scalar.input)?;

        let input_schema = eval_scalar.input.output_schema()?;
        let exprs = eval_scalar
            .exprs
            .iter()
            .map(|(scalar, _)| scalar.as_expr(&BUILTIN_FUNCTIONS))
            .collect::<Vec<_>>();

        if exprs.is_empty() {
            return Ok(());
        }

        let op = BlockOperator::Map {
            exprs,
            projections: Some(eval_scalar.projections.clone()),
        };

        let num_input_columns = input_schema.num_fields();

        self.main_pipeline.add_transform(|input, output| {
            let transform = CompoundBlockOperator::new(
                vec![op.clone()],
                self.func_ctx.clone(),
                num_input_columns,
            );

            if self.enable_profiling {
                Ok(ProcessorPtr::create(TransformProfileWrapper::create(
                    transform,
                    input,
                    output,
                    eval_scalar.plan_id,
                    self.proc_profs.clone(),
                )))
            } else {
                Ok(ProcessorPtr::create(Transformer::create(
                    input, output, transform,
                )))
            }
        })?;

        Ok(())
    }

    fn build_project_set(&mut self, project_set: &ProjectSet) -> Result<()> {
        self.build_pipeline(&project_set.input)?;

        let op = BlockOperator::FlatMap {
            projections: project_set.projections.clone(),
            srf_exprs: project_set
                .srf_exprs
                .iter()
                .map(|(expr, _)| expr.as_expr(&BUILTIN_FUNCTIONS))
                .collect(),
        };

        let num_input_columns = project_set.input.output_schema()?.num_fields();

        self.main_pipeline.add_transform(|input, output| {
            let transform = CompoundBlockOperator::new(
                vec![op.clone()],
                self.func_ctx.clone(),
                num_input_columns,
            );

            if self.enable_profiling {
                Ok(ProcessorPtr::create(TransformProfileWrapper::create(
                    transform,
                    input,
                    output,
                    project_set.plan_id,
                    self.proc_profs.clone(),
                )))
            } else {
                Ok(ProcessorPtr::create(Transformer::create(
                    input, output, transform,
                )))
            }
        })
    }

    fn build_lambda(&mut self, lambda: &Lambda) -> Result<()> {
        self.build_pipeline(&lambda.input)?;

        let funcs = lambda.lambda_funcs.clone();
        let op = BlockOperator::LambdaMap { funcs };

        let input_schema = lambda.input.output_schema()?;
        let num_input_columns = input_schema.num_fields();

        self.main_pipeline.add_transform(|input, output| {
            let transform = CompoundBlockOperator::new(
                vec![op.clone()],
                self.func_ctx.clone(),
                num_input_columns,
            );

            if self.enable_profiling {
                Ok(ProcessorPtr::create(TransformProfileWrapper::create(
                    transform,
                    input,
                    output,
                    lambda.plan_id,
                    self.proc_profs.clone(),
                )))
            } else {
                Ok(ProcessorPtr::create(Transformer::create(
                    input, output, transform,
                )))
            }
        })?;

        Ok(())
    }

    fn build_aggregate_expand(&mut self, expand: &AggregateExpand) -> Result<()> {
        self.build_pipeline(&expand.input)?;
        let input_schema = expand.input.output_schema()?;
        let group_bys = expand
            .group_bys
            .iter()
            .take(expand.group_bys.len() - 1) // The last group-by will be virtual column `_grouping_id`
            .map(|i| input_schema.index_of(&i.to_string()))
            .collect::<Result<Vec<_>>>()?;
        let grouping_sets = expand
            .grouping_sets
            .sets
            .iter()
            .map(|sets| {
                sets.iter()
                    .map(|i| {
                        let i = input_schema.index_of(&i.to_string())?;
                        let offset = group_bys.iter().position(|j| *j == i).unwrap();
                        Ok(offset)
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .collect::<Result<Vec<_>>>()?;
        let mut grouping_ids = Vec::with_capacity(grouping_sets.len());
        let mask = (1 << group_bys.len()) - 1;
        for set in grouping_sets {
            let mut id = 0;
            for i in set {
                id |= 1 << i;
            }
            // For element in `group_bys`,
            // if it is in current grouping set: set 0, else: set 1. (1 represents it will be NULL in grouping)
            // Example: GROUP BY GROUPING SETS ((a, b), (a), (b), ())
            // group_bys: [a, b]
            // grouping_sets: [[0, 1], [0], [1], []]
            // grouping_ids: 00, 01, 10, 11
            grouping_ids.push(!id & mask);
        }

        self.main_pipeline.add_transform(|input, output| {
            Ok(TransformExpandGroupingSets::create(
                input,
                output,
                group_bys.clone(),
                grouping_ids.clone(),
            ))
        })
    }

    fn build_aggregate_partial(&mut self, aggregate: &AggregatePartial) -> Result<()> {
        self.build_pipeline(&aggregate.input)?;

        let params = Self::build_aggregator_params(
            aggregate.input.output_schema()?,
            &aggregate.group_by,
            &aggregate.agg_funcs,
            None,
        )?;

        if params.group_columns.is_empty() {
            return self.main_pipeline.add_transform(|input, output| {
                let transform = PartialSingleStateAggregator::try_create(input, output, &params)?;

                if self.enable_profiling {
                    Ok(ProcessorPtr::create(ProcessorProfileWrapper::create(
                        transform,
                        aggregate.plan_id,
                        self.proc_profs.clone(),
                    )))
                } else {
                    Ok(ProcessorPtr::create(transform))
                }
            });
        }

        let efficiently_memory = self.settings.get_efficiently_memory_group_by()?;

        let group_cols = &params.group_columns;
        let schema_before_group_by = params.input_schema.clone();
        let sample_block = DataBlock::empty_with_schema(schema_before_group_by);
        let method = DataBlock::choose_hash_method(&sample_block, group_cols, efficiently_memory)?;

        self.main_pipeline.add_transform(|input, output| {
            let transform = match params.aggregate_functions.is_empty() {
                true => with_mappedhash_method!(|T| match method.clone() {
                    HashMethodKind::T(method) => TransformPartialGroupBy::try_create(
                        self.ctx.clone(),
                        method,
                        input,
                        output,
                        params.clone()
                    ),
                }),
                false => with_mappedhash_method!(|T| match method.clone() {
                    HashMethodKind::T(method) => TransformPartialAggregate::try_create(
                        self.ctx.clone(),
                        method,
                        input,
                        output,
                        params.clone()
                    ),
                }),
            }?;

            if self.enable_profiling {
                Ok(ProcessorPtr::create(ProcessorProfileWrapper::create(
                    transform,
                    aggregate.plan_id,
                    self.proc_profs.clone(),
                )))
            } else {
                Ok(ProcessorPtr::create(transform))
            }
        })?;

        // If cluster mode, spill write will be completed in exchange serialize, because we need scatter the block data first
        if self.ctx.get_cluster().is_empty() {
            let operator = DataOperator::instance().operator();
            let location_prefix = query_spill_prefix(&self.ctx.get_tenant());
            self.main_pipeline.add_transform(|input, output| {
                let transform = match params.aggregate_functions.is_empty() {
                    true => with_mappedhash_method!(|T| match method.clone() {
                        HashMethodKind::T(method) => TransformGroupBySpillWriter::create(
                            self.ctx.clone(),
                            input,
                            output,
                            method,
                            operator.clone(),
                            location_prefix.clone()
                        ),
                    }),
                    false => with_mappedhash_method!(|T| match method.clone() {
                        HashMethodKind::T(method) => TransformAggregateSpillWriter::create(
                            self.ctx.clone(),
                            input,
                            output,
                            method,
                            operator.clone(),
                            params.clone(),
                            location_prefix.clone()
                        ),
                    }),
                };

                if self.enable_profiling {
                    Ok(ProcessorPtr::create(ProcessorProfileWrapper::create(
                        transform,
                        aggregate.plan_id,
                        self.proc_profs.clone(),
                    )))
                } else {
                    Ok(ProcessorPtr::create(transform))
                }
            })?;
        }

        self.exchange_injector = match params.aggregate_functions.is_empty() {
            true => with_mappedhash_method!(|T| match method.clone() {
                HashMethodKind::T(method) =>
                    AggregateInjector::<_, ()>::create(self.ctx.clone(), method, params.clone()),
            }),
            false => with_mappedhash_method!(|T| match method.clone() {
                HashMethodKind::T(method) =>
                    AggregateInjector::<_, usize>::create(self.ctx.clone(), method, params.clone()),
            }),
        };

        Ok(())
    }

    fn build_aggregate_final(&mut self, aggregate: &AggregateFinal) -> Result<()> {
        let params = Self::build_aggregator_params(
            aggregate.before_group_by_schema.clone(),
            &aggregate.group_by,
            &aggregate.agg_funcs,
            aggregate.limit,
        )?;

        if params.group_columns.is_empty() {
            self.build_pipeline(&aggregate.input)?;
            self.main_pipeline.try_resize(1)?;
            self.main_pipeline.add_transform(|input, output| {
                let transform = FinalSingleStateAggregator::try_create(input, output, &params)?;

                if self.enable_profiling {
                    Ok(ProcessorPtr::create(ProcessorProfileWrapper::create(
                        transform,
                        aggregate.plan_id,
                        self.proc_profs.clone(),
                    )))
                } else {
                    Ok(ProcessorPtr::create(transform))
                }
            })?;

            // Append a profile stub to record the output rows and bytes
            if self.enable_profiling {
                self.main_pipeline.add_transform(|input, output| {
                    Ok(ProcessorPtr::create(Transformer::create(
                        input,
                        output,
                        ProfileStub::new(aggregate.plan_id, self.proc_profs.clone())
                            .accumulate_output_rows()
                            .accumulate_output_bytes(),
                    )))
                })?;
            }

            return Ok(());
        }

        let efficiently_memory = self.settings.get_efficiently_memory_group_by()?;

        let group_cols = &params.group_columns;
        let schema_before_group_by = params.input_schema.clone();
        let sample_block = DataBlock::empty_with_schema(schema_before_group_by);
        let method = DataBlock::choose_hash_method(&sample_block, group_cols, efficiently_memory)?;

        let old_inject = self.exchange_injector.clone();

        match params.aggregate_functions.is_empty() {
            true => with_hash_method!(|T| match method {
                HashMethodKind::T(v) => {
                    let input: &PhysicalPlan = &aggregate.input;
                    if matches!(input, PhysicalPlan::ExchangeSource(_)) {
                        self.exchange_injector = AggregateInjector::<_, ()>::create(
                            self.ctx.clone(),
                            v.clone(),
                            params.clone(),
                        );
                    }

                    self.build_pipeline(&aggregate.input)?;
                    self.exchange_injector = old_inject;
                    build_partition_bucket::<_, ()>(
                        v,
                        &mut self.main_pipeline,
                        params.clone(),
                        self.enable_profiling,
                        aggregate.plan_id,
                        self.proc_profs.clone(),
                    )
                }
            }),
            false => with_hash_method!(|T| match method {
                HashMethodKind::T(v) => {
                    let input: &PhysicalPlan = &aggregate.input;
                    if matches!(input, PhysicalPlan::ExchangeSource(_)) {
                        self.exchange_injector = AggregateInjector::<_, usize>::create(
                            self.ctx.clone(),
                            v.clone(),
                            params.clone(),
                        );
                    }
                    self.build_pipeline(&aggregate.input)?;
                    self.exchange_injector = old_inject;
                    build_partition_bucket::<_, usize>(
                        v,
                        &mut self.main_pipeline,
                        params.clone(),
                        self.enable_profiling,
                        aggregate.plan_id,
                        self.proc_profs.clone(),
                    )
                }
            }),
        }
    }

    pub fn build_aggregator_params(
        input_schema: DataSchemaRef,
        group_by: &[IndexType],
        agg_funcs: &[AggregateFunctionDesc],
        limit: Option<usize>,
    ) -> Result<Arc<AggregatorParams>> {
        let mut agg_args = Vec::with_capacity(agg_funcs.len());
        let (group_by, group_data_types) = group_by
            .iter()
            .map(|i| {
                let index = input_schema.index_of(&i.to_string())?;
                Ok((index, input_schema.field(index).data_type().clone()))
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .unzip::<_, _, Vec<_>, Vec<_>>();

        let aggs: Vec<AggregateFunctionRef> = agg_funcs
            .iter()
            .map(|agg_func| {
                let args = agg_func
                    .arg_indices
                    .iter()
                    .map(|i| {
                        let index = input_schema.index_of(&i.to_string())?;
                        Ok(index)
                    })
                    .collect::<Result<Vec<_>>>()?;
                agg_args.push(args);
                AggregateFunctionFactory::instance().get(
                    agg_func.sig.name.as_str(),
                    agg_func.sig.params.clone(),
                    agg_func.sig.args.clone(),
                )
            })
            .collect::<Result<_>>()?;

        let params = AggregatorParams::try_create(
            input_schema,
            group_data_types,
            &group_by,
            &aggs,
            &agg_args,
            limit,
        )?;

        Ok(params)
    }

    fn build_window(&mut self, window: &Window) -> Result<()> {
        self.build_pipeline(&window.input)?;

        let input_schema = window.input.output_schema()?;

        let partition_by = window
            .partition_by
            .iter()
            .map(|p| {
                let offset = input_schema.index_of(&p.to_string())?;
                Ok(offset)
            })
            .collect::<Result<Vec<_>>>()?;

        let order_by = window
            .order_by
            .iter()
            .map(|o| {
                let offset = input_schema.index_of(&o.order_by.to_string())?;
                Ok(SortColumnDescription {
                    offset,
                    asc: o.asc,
                    nulls_first: o.nulls_first,
                    is_nullable: input_schema.field(offset).is_nullable(), // Used for check null frame.
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let old_output_len = self.main_pipeline.output_len();
        if !partition_by.is_empty() || !order_by.is_empty() {
            let mut sort_desc = Vec::with_capacity(partition_by.len() + order_by.len());

            for offset in &partition_by {
                sort_desc.push(SortColumnDescription {
                    offset: *offset,
                    asc: true,
                    nulls_first: true,
                    is_nullable: input_schema.field(*offset).is_nullable(),  // This information is not needed here.
                })
            }

            sort_desc.extend(order_by.clone());

            self.build_sort_pipeline(input_schema.clone(), sort_desc, window.plan_id, None, false)?;
        }
        // `TransformWindow` is a pipeline breaker.
        self.main_pipeline.try_resize(1)?;
        let func = WindowFunctionInfo::try_create(&window.func, &input_schema)?;
        // Window
        self.main_pipeline.add_transform(|input, output| {
            // The transform can only be created here, because it cannot be cloned.

            let transform = if window.window_frame.units.is_rows() {
                let start_bound = FrameBound::try_from(&window.window_frame.start_bound)?;
                let end_bound = FrameBound::try_from(&window.window_frame.end_bound)?;
                Box::new(TransformWindow::<u64>::try_create_rows(
                    input,
                    output,
                    func.clone(),
                    partition_by.clone(),
                    order_by.clone(),
                    (start_bound, end_bound),
                )?) as Box<dyn Processor>
            } else {
                if order_by.len() == 1 {
                    // If the length of order_by is 1, there may be a RANGE frame.
                    let data_type = input_schema
                        .field(order_by[0].offset)
                        .data_type()
                        .remove_nullable();
                    with_number_mapped_type!(|NUM_TYPE| match data_type {
                        DataType::Number(NumberDataType::NUM_TYPE) => {
                            let start_bound =
                                FrameBound::try_from(&window.window_frame.start_bound)?;
                            let end_bound = FrameBound::try_from(&window.window_frame.end_bound)?;
                            return Ok(ProcessorPtr::create(Box::new(
                                TransformWindow::<NUM_TYPE>::try_create_range(
                                    input,
                                    output,
                                    func.clone(),
                                    partition_by.clone(),
                                    order_by.clone(),
                                    (start_bound, end_bound),
                                )?,
                            )
                                as Box<dyn Processor>));
                        }
                        _ => {}
                    })
                }

                // There is no offset in the RANGE frame. (just CURRENT ROW or UNBOUNDED)
                // So we can use any number type to create the transform.
                let start_bound = FrameBound::try_from(&window.window_frame.start_bound)?;
                let end_bound = FrameBound::try_from(&window.window_frame.end_bound)?;
                Box::new(TransformWindow::<u8>::try_create_range(
                    input,
                    output,
                    func.clone(),
                    partition_by.clone(),
                    order_by.clone(),
                    (start_bound, end_bound),
                )?) as Box<dyn Processor>
            };
            Ok(ProcessorPtr::create(transform))
        })?;

        self.main_pipeline.try_resize(old_output_len)
    }

    fn build_sort(&mut self, sort: &Sort) -> Result<()> {
        self.build_pipeline(&sort.input)?;

        let input_schema = sort.input.output_schema()?;

        if let Some(proj) = &sort.pre_projection {
            // Do projection to reduce useless data copying during sorting.
            let projection = proj
                .iter()
                .filter_map(|i| input_schema.index_of(&i.to_string()).ok())
                .collect::<Vec<_>>();

            if projection.len() < input_schema.fields().len() {
                // Only if the projection is not a full projection, we need to add a projection transform.
                self.main_pipeline.add_transform(|input, output| {
                    Ok(ProcessorPtr::create(CompoundBlockOperator::create(
                        input,
                        output,
                        input_schema.num_fields(),
                        self.func_ctx.clone(),
                        vec![BlockOperator::Project {
                            projection: projection.clone(),
                        }],
                    )))
                })?;
            }
        }

        let input_schema = sort.output_schema()?;

        let sort_desc = sort
            .order_by
            .iter()
            .map(|desc| {
                let offset = input_schema.index_of(&desc.order_by.to_string())?;
                Ok(SortColumnDescription {
                    offset,
                    asc: desc.asc,
                    nulls_first: desc.nulls_first,
                    is_nullable: input_schema.field(offset).is_nullable(),  // This information is not needed here.
                })
            })
            .collect::<Result<Vec<_>>>()?;

        self.build_sort_pipeline(
            input_schema,
            sort_desc,
            sort.plan_id,
            sort.limit,
            sort.after_exchange,
        )
    }

    fn build_sort_pipeline(
        &mut self,
        input_schema: DataSchemaRef,
        sort_desc: Vec<SortColumnDescription>,
        plan_id: u32,
        limit: Option<usize>,
        after_exchange: bool,
    ) -> Result<()> {
        let block_size = self.settings.get_max_block_size()? as usize;
        let max_threads = self.settings.get_max_threads()? as usize;

        // TODO(Winter): the query will hang in MultiSortMergeProcessor when max_threads == 1 and output_len != 1
        if self.main_pipeline.output_len() == 1 || max_threads == 1 {
            self.main_pipeline.try_resize(max_threads)?;
        }
        let prof_info = if self.enable_profiling {
            Some((plan_id, self.proc_profs.clone()))
        } else {
            None
        };

        build_full_sort_pipeline(
            &mut self.main_pipeline,
            input_schema,
            sort_desc,
            limit,
            block_size,
            block_size,
            prof_info,
            after_exchange,
        )
    }

    fn build_limit(&mut self, limit: &Limit) -> Result<()> {
        self.build_pipeline(&limit.input)?;

        if limit.limit.is_some() || limit.offset != 0 {
            self.main_pipeline.try_resize(1)?;
            return self.main_pipeline.add_transform(|input, output| {
                let transform =
                    TransformLimit::try_create(limit.limit, limit.offset, input, output)?;

                if self.enable_profiling {
                    Ok(ProcessorPtr::create(ProcessorProfileWrapper::create(
                        transform,
                        limit.plan_id,
                        self.proc_profs.clone(),
                    )))
                } else {
                    Ok(ProcessorPtr::create(transform))
                }
            });
        }
        Ok(())
    }

    fn build_row_fetch(&mut self, row_fetch: &RowFetch) -> Result<()> {
        debug_assert!(matches!(&*row_fetch.input, PhysicalPlan::Limit(_)));
        self.build_pipeline(&row_fetch.input)?;
        build_row_fetcher_pipeline(
            self.ctx.clone(),
            &mut self.main_pipeline,
            row_fetch.row_id_col_offset,
            &row_fetch.source,
            row_fetch.cols_to_fetch.clone(),
        )
    }

    fn build_join_probe(&mut self, join: &HashJoin, state: Arc<HashJoinState>) -> Result<()> {
        self.build_pipeline(&join.probe)?;

        let max_block_size = self.settings.get_max_block_size()? as usize;
        let barrier = Barrier::new(self.main_pipeline.output_len());
        let restore_barrier = Barrier::new(self.main_pipeline.output_len());
        let probe_state = Arc::new(HashJoinProbeState::create(
            self.ctx.clone(),
            self.func_ctx.clone(),
            state,
            &join.probe_projections,
            &join.probe_keys,
            join.probe.output_schema()?,
            &join.join_type,
            self.main_pipeline.output_len(),
            barrier,
            restore_barrier,
        )?);
        let mut has_string_column = false;
        for filed in join.output_schema()?.fields() {
            has_string_column |= filed.data_type().is_string_column();
        }

        self.main_pipeline.add_transform(|input, output| {
            let probe_spill_state = if self.settings.get_join_spilling_threshold()? != 0 {
                Some(Box::new(ProbeSpillState::create(
                    self.ctx.clone(),
                    probe_state.clone(),
                )))
            } else {
                None
            };
            let transform = TransformHashJoinProbe::create(
                input,
                output,
                join.projections.clone(),
                probe_state.clone(),
                probe_spill_state,
                max_block_size,
                self.func_ctx.clone(),
                &join.join_type,
                !join.non_equi_conditions.is_empty(),
                has_string_column,
            )?;

            if self.enable_profiling {
                Ok(ProcessorPtr::create(ProcessorProfileWrapper::create(
                    transform,
                    join.plan_id,
                    self.proc_profs.clone(),
                )))
            } else {
                Ok(ProcessorPtr::create(transform))
            }
        })?;

        if self.enable_profiling {
            // Add a stub after the probe processor to accumulate the output rows.
            self.main_pipeline.add_transform(|input, output| {
                Ok(ProcessorPtr::create(Transformer::create(
                    input,
                    output,
                    ProfileStub::new(join.plan_id, self.proc_profs.clone())
                        .accumulate_output_rows()
                        .accumulate_output_bytes(),
                )))
            })?;
        }

        Ok(())
    }

    pub fn build_exchange_source(&mut self, exchange_source: &ExchangeSource) -> Result<()> {
        let exchange_manager = self.ctx.get_exchange_manager();
        let build_res = exchange_manager.get_fragment_source(
            &exchange_source.query_id,
            exchange_source.source_fragment_id,
            self.enable_profiling,
            self.exchange_injector.clone(),
        )?;
        self.main_pipeline = build_res.main_pipeline;
        self.pipelines.extend(build_res.sources_pipelines);
        Ok(())
    }

    pub fn build_exchange_sink(&mut self, exchange_sink: &ExchangeSink) -> Result<()> {
        // ExchangeSink will be appended by `ExchangeManager::execute_pipeline`
        self.build_pipeline(&exchange_sink.input)
    }

    fn expand_union_all(
        &mut self,
        input: &PhysicalPlan,
        union_plan: &UnionAll,
    ) -> Result<Receiver<DataBlock>> {
        let union_ctx = QueryContext::create_from(self.ctx.clone());
        let mut pipeline_builder = PipelineBuilder::create(
            self.func_ctx.clone(),
            self.settings.clone(),
            union_ctx,
            self.enable_profiling,
            self.proc_profs.clone(),
        );
        pipeline_builder.cte_state = self.cte_state.clone();
        let mut build_res = pipeline_builder.finalize(input)?;

        assert!(build_res.main_pipeline.is_pulling_pipeline()?);

        let (tx, rx) = async_channel::unbounded();

        build_res.main_pipeline.add_sink(|input_port| {
            let transform = UnionReceiveSink::create(Some(tx.clone()), input_port);

            if self.enable_profiling {
                Ok(ProcessorPtr::create(ProcessorProfileWrapper::create(
                    transform,
                    union_plan.plan_id,
                    self.proc_profs.clone(),
                )))
            } else {
                Ok(ProcessorPtr::create(transform))
            }
        })?;

        self.pipelines.push(build_res.main_pipeline);
        self.pipelines
            .extend(build_res.sources_pipelines.into_iter());
        Ok(rx)
    }

    pub fn build_union_all(&mut self, union_all: &UnionAll) -> Result<()> {
        self.build_pipeline(&union_all.left)?;
        let union_all_receiver = self.expand_union_all(&union_all.right, union_all)?;
        self.main_pipeline
            .add_transform(|transform_input_port, transform_output_port| {
                let transform = TransformMergeBlock::try_create(
                    transform_input_port,
                    transform_output_port,
                    union_all.left.output_schema()?,
                    union_all.right.output_schema()?,
                    union_all.pairs.clone(),
                    union_all_receiver.clone(),
                )?;

                if self.enable_profiling {
                    Ok(ProcessorPtr::create(ProcessorProfileWrapper::create(
                        transform,
                        union_all.plan_id,
                        self.proc_profs.clone(),
                    )))
                } else {
                    Ok(ProcessorPtr::create(transform))
                }
            })?;
        Ok(())
    }

    pub fn build_distributed_insert_select(
        &mut self,
        insert_select: &DistributedInsertSelect,
    ) -> Result<()> {
        let select_schema = &insert_select.select_schema;
        let insert_schema = &insert_select.insert_schema;

        self.build_pipeline(&insert_select.input)?;

        // should render result for select
        PipelineBuilder::render_result_set(
            &self.func_ctx,
            insert_select.input.output_schema()?,
            &insert_select.select_column_bindings,
            &mut self.main_pipeline,
            false,
        )?;

        if insert_select.cast_needed {
            self.main_pipeline
                .add_transform(|transform_input_port, transform_output_port| {
                    TransformCastSchema::try_create(
                        transform_input_port,
                        transform_output_port,
                        select_schema.clone(),
                        insert_schema.clone(),
                        self.func_ctx.clone(),
                    )
                })?;
        }

        let table = self.ctx.build_table_by_table_info(
            &insert_select.catalog_info,
            &insert_select.table_info,
            None,
        )?;

        let source_schema = insert_schema;
        build_fill_missing_columns_pipeline(
            self.ctx.clone(),
            &mut self.main_pipeline,
            table.clone(),
            source_schema.clone(),
        )?;

        table.append_data(
            self.ctx.clone(),
            &mut self.main_pipeline,
            AppendMode::Normal,
        )?;

        Ok(())
    }

    pub fn build_runtime_filter_source(
        &mut self,
        runtime_filter_source: &RuntimeFilterSource,
    ) -> Result<()> {
        let state = self.build_runtime_filter_state(self.ctx.clone(), runtime_filter_source)?;
        self.expand_runtime_filter_source(&runtime_filter_source.right_side, state.clone())?;
        self.build_runtime_filter(&runtime_filter_source.left_side, state)?;
        Ok(())
    }

    fn expand_runtime_filter_source(
        &mut self,
        _right_side: &PhysicalPlan,
        state: Arc<RuntimeFilterState>,
    ) -> Result<()> {
        let pipeline = &mut self.pipelines[self.index.unwrap()];
        let output_size = pipeline.output_len();
        debug_assert!(output_size % 2 == 0);

        let mut items = Vec::with_capacity(output_size);
        //           Join
        //          /   \
        //        /      \
        //   RFSource     \
        //      /    \     \
        //     /      \     \
        // scan t1     scan t2
        for _ in 0..output_size / 2 {
            let input = InputPort::create();
            items.push(PipeItem::create(
                ProcessorPtr::create(TransformHashJoinBuild::try_create(
                    input.clone(),
                    self.join_state.as_ref().unwrap().clone(),
                    None,
                )?),
                vec![input],
                vec![],
            ));
            let input = InputPort::create();
            items.push(PipeItem::create(
                ProcessorPtr::create(Sinker::<SinkRuntimeFilterSource>::create(
                    input.clone(),
                    SinkRuntimeFilterSource::new(state.clone()),
                )),
                vec![input],
                vec![],
            ));
        }
        pipeline.add_pipe(Pipe::create(output_size, 0, items));
        Ok(())
    }

    fn build_runtime_filter(
        &mut self,
        left_side: &PhysicalPlan,
        state: Arc<RuntimeFilterState>,
    ) -> Result<()> {
        self.build_pipeline(left_side)?;
        self.main_pipeline.add_transform(|input, output| {
            let processor = TransformRuntimeFilter::create(input, output, state.clone());
            Ok(ProcessorPtr::create(processor))
        })?;
        Ok(())
    }

    fn build_runtime_filter_state(
        &self,
        ctx: Arc<QueryContext>,
        runtime_filter_source: &RuntimeFilterSource,
    ) -> Result<Arc<RuntimeFilterState>> {
        Ok(Arc::new(RuntimeFilterState::new(
            ctx,
            runtime_filter_source.left_runtime_filters.clone(),
            runtime_filter_source.right_runtime_filters.clone(),
        )))
    }

    fn build_materialized_cte(&mut self, materialized_cte: &MaterializedCte) -> Result<()> {
        self.expand_left_side_pipeline(
            &materialized_cte.left,
            materialized_cte.cte_idx,
            &materialized_cte.left_output_columns,
        )?;
        self.build_pipeline(&materialized_cte.right)
    }

    fn expand_left_side_pipeline(
        &mut self,
        left_side: &PhysicalPlan,
        cte_idx: IndexType,
        left_output_columns: &[ColumnBinding],
    ) -> Result<()> {
        let left_side_ctx = QueryContext::create_from(self.ctx.clone());
        let state = Arc::new(MaterializedCteState::new(self.ctx.clone()));
        self.cte_state.insert(cte_idx, state.clone());
        let mut left_side_builder = PipelineBuilder::create(
            self.func_ctx.clone(),
            self.settings.clone(),
            left_side_ctx,
            self.enable_profiling,
            self.proc_profs.clone(),
        );
        left_side_builder.cte_state = self.cte_state.clone();
        let mut left_side_pipeline = left_side_builder.finalize(left_side)?;
        assert!(left_side_pipeline.main_pipeline.is_pulling_pipeline()?);

        PipelineBuilder::render_result_set(
            &self.func_ctx,
            left_side.output_schema()?,
            left_output_columns,
            &mut left_side_pipeline.main_pipeline,
            false,
        )?;

        left_side_pipeline.main_pipeline.add_sink(|input| {
            let transform = Sinker::<MaterializedCteSink>::create(
                input,
                MaterializedCteSink::create(self.ctx.clone(), cte_idx, state.clone())?,
            );
            Ok(ProcessorPtr::create(transform))
        })?;
        self.pipelines.push(left_side_pipeline.main_pipeline);
        self.pipelines
            .extend(left_side_pipeline.sources_pipelines.into_iter());
        Ok(())
    }
}

pub struct ValueSource {
    data: String,
    ctx: Arc<dyn TableContext>,
    name_resolution_ctx: NameResolutionContext,
    bind_context: BindContext,
    schema: DataSchemaRef,
    metadata: MetadataRef,
    start: usize,
    is_finished: bool,
}

#[async_trait::async_trait]
impl AsyncSource for ValueSource {
    const NAME: &'static str = "ValueSource";
    const SKIP_EMPTY_DATA_BLOCK: bool = true;

    #[async_trait::unboxed_simple]
    #[async_backtrace::framed]
    async fn generate(&mut self) -> Result<Option<DataBlock>> {
        if self.is_finished {
            return Ok(None);
        }

        let format = self.ctx.get_format_settings()?;
        let field_decoder = FastFieldDecoderValues::create_for_insert(format);

        let mut values_decoder = FastValuesDecoder::new(&self.data, &field_decoder);
        let estimated_rows = values_decoder.estimated_rows();

        let mut columns = self
            .schema
            .fields()
            .iter()
            .map(|f| ColumnBuilder::with_capacity(f.data_type(), estimated_rows))
            .collect::<Vec<_>>();

        values_decoder.parse(&mut columns, self).await?;

        let columns = columns
            .into_iter()
            .map(|col| col.build())
            .collect::<Vec<_>>();
        let block = DataBlock::new_from_columns(columns);
        self.is_finished = true;
        Ok(Some(block))
    }
}

#[async_trait::async_trait]
impl FastValuesDecodeFallback for ValueSource {
    async fn parse_fallback(&self, sql: &str) -> Result<Vec<Scalar>> {
        let res: Result<Vec<Scalar>> = try {
            let settings = self.ctx.get_settings();
            let sql_dialect = settings.get_sql_dialect()?;
            let tokens = tokenize_sql(sql)?;
            let mut bind_context = self.bind_context.clone();
            let metadata = self.metadata.clone();

            let exprs = parse_comma_separated_exprs(&tokens[1..tokens.len()], sql_dialect)?;
            bind_context
                .exprs_to_scalar(
                    exprs,
                    &self.schema,
                    self.ctx.clone(),
                    &self.name_resolution_ctx,
                    metadata,
                )
                .await?
        };
        res.map_err(|mut err| {
            // The input for ValueSource is a sub-section of the original SQL. This causes
            // the error span to have an offset, so we adjust the span accordingly.
            if let Some(span) = err.span() {
                err = err.set_span(Some(
                    (span.start() + self.start..span.end() + self.start).into(),
                ));
            }
            err
        })
    }
}

impl ValueSource {
    pub fn new(
        data: String,
        ctx: Arc<dyn TableContext>,
        name_resolution_ctx: NameResolutionContext,
        schema: DataSchemaRef,
        start: usize,
    ) -> Self {
        let bind_context = BindContext::new();
        let metadata = Arc::new(RwLock::new(Metadata::default()));

        Self {
            data,
            ctx,
            name_resolution_ctx,
            schema,
            bind_context,
            metadata,
            start,
            is_finished: false,
        }
    }
}
