// Copyright 2023 Datafuse Labs.
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
use common_exception::Result;
use common_pipeline_core::pipe::Pipe;
use common_pipeline_core::pipe::PipeItem;
use common_pipeline_core::processors::port::InputPort;
use common_pipeline_core::processors::port::OutputPort;
use common_pipeline_core::Pipeline;

use crate::api::rpc::exchange::exchange_params::MergeExchangeParams;
use crate::api::rpc::exchange::serde::exchange_deserializer::TransformExchangeDeserializer;
use crate::api::rpc::exchange::serde::exchange_serializer::TransformExchangeSerializer;
use crate::api::rpc::exchange::serde::exchange_serializer::TransformScatterExchangeSerializer;
use crate::api::rpc::flight_scatter::FlightScatter;
use crate::api::BroadcastFlightScatter;
use crate::api::DataExchange;
use crate::api::ExchangeSorting;
use crate::api::HashFlightScatter;
use crate::api::ShuffleExchangeParams;
use crate::pipelines::processors::TransformDummy;
use crate::sessions::QueryContext;

pub trait ExchangeInjector: Send + Sync + 'static {
    fn flight_scatter(
        &self,
        ctx: &Arc<QueryContext>,
        exchange: &DataExchange,
    ) -> Result<Arc<Box<dyn FlightScatter>>>;

    fn exchange_sorting(&self) -> Option<Arc<dyn ExchangeSorting>>;

    fn apply_merge_serializer(
        &self,
        params: &MergeExchangeParams,
        pipeline: &mut Pipeline,
    ) -> Result<()>;

    fn apply_shuffle_serializer(
        &self,
        params: &ShuffleExchangeParams,
        pipeline: &mut Pipeline,
    ) -> Result<()>;

    fn apply_merge_deserializer(
        &self,
        remote_inputs: usize,
        params: &MergeExchangeParams,
        pipeline: &mut Pipeline,
    ) -> Result<()>;

    fn apply_shuffle_deserializer(
        &self,
        remote_inputs: usize,
        params: &ShuffleExchangeParams,
        pipeline: &mut Pipeline,
    ) -> Result<()>;
}

pub struct DefaultExchangeInjector;

impl DefaultExchangeInjector {
    pub fn create() -> Arc<dyn ExchangeInjector> {
        Arc::new(DefaultExchangeInjector {})
    }
}

impl ExchangeInjector for DefaultExchangeInjector {
    fn flight_scatter(
        &self,
        ctx: &Arc<QueryContext>,
        exchange: &DataExchange,
    ) -> Result<Arc<Box<dyn FlightScatter>>> {
        Ok(Arc::new(match exchange {
            DataExchange::Merge(_) => unreachable!(),
            DataExchange::Broadcast(exchange) => Box::new(BroadcastFlightScatter::try_create(
                exchange.destination_ids.len(),
            )?),
            DataExchange::ShuffleDataExchange(exchange) => HashFlightScatter::try_create(
                ctx.get_function_context()?,
                exchange.shuffle_keys.clone(),
                exchange.destination_ids.len(),
            )?,
        }))
    }

    fn exchange_sorting(&self) -> Option<Arc<dyn ExchangeSorting>> {
        None
    }

    fn apply_merge_serializer(
        &self,
        params: &MergeExchangeParams,
        pipeline: &mut Pipeline,
    ) -> Result<()> {
        pipeline.add_transform(|input, output| {
            TransformExchangeSerializer::create(input, output, params, None)
        })
    }

    fn apply_shuffle_serializer(
        &self,
        params: &ShuffleExchangeParams,
        pipeline: &mut Pipeline,
    ) -> Result<()> {
        pipeline.add_transform(|input, output| {
            TransformScatterExchangeSerializer::create(input, output, params, None)
        })
    }

    fn apply_merge_deserializer(
        &self,
        remote_inputs: usize,
        params: &MergeExchangeParams,
        pipeline: &mut Pipeline,
    ) -> Result<()> {
        let local_inputs = pipeline.output_len() - remote_inputs;
        let mut items = Vec::with_capacity(pipeline.output_len());

        for _index in 0..local_inputs {
            let input = InputPort::create();
            let output = OutputPort::create();

            items.push(PipeItem::create(
                TransformDummy::create(input.clone(), output.clone()),
                vec![input],
                vec![output],
            ));
        }

        for _index in 0..remote_inputs {
            let input = InputPort::create();
            let output = OutputPort::create();

            let schema = &params.schema;
            items.push(PipeItem::create(
                TransformExchangeDeserializer::create(input.clone(), output.clone(), schema),
                vec![input],
                vec![output],
            ));
        }

        pipeline.add_pipe(Pipe::create(items.len(), items.len(), items));
        Ok(())
    }

    fn apply_shuffle_deserializer(
        &self,
        remote_inputs: usize,
        params: &ShuffleExchangeParams,
        pipeline: &mut Pipeline,
    ) -> Result<()> {
        let local_inputs = pipeline.output_len() - remote_inputs;

        let mut items = Vec::with_capacity(pipeline.output_len());

        for _index in 0..local_inputs {
            let input = InputPort::create();
            let output = OutputPort::create();

            items.push(PipeItem::create(
                TransformDummy::create(input.clone(), output.clone()),
                vec![input],
                vec![output],
            ));
        }

        for _index in 0..remote_inputs {
            let input = InputPort::create();
            let output = OutputPort::create();

            let schema = &params.schema;
            items.push(PipeItem::create(
                TransformExchangeDeserializer::create(input.clone(), output.clone(), schema),
                vec![input],
                vec![output],
            ));
        }

        pipeline.add_pipe(Pipe::create(items.len(), items.len(), items));

        Ok(())
    }
}
