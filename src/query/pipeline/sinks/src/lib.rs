// Copyright 2021 Datafuse Labs.
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

#![feature(type_alias_impl_trait)]

mod async_sink;
mod context_sink;
mod empty_sink;
mod sync_sink;
mod sync_sink_sender;
mod union_receive_sink;

pub use async_sink::AsyncSink;
pub use async_sink::AsyncSinker;
pub use context_sink::ContextSink;
pub use empty_sink::EmptySink;
pub use sync_sink::Sink;
pub use sync_sink::Sinker;
pub use sync_sink_sender::SyncSenderSink;
pub use union_receive_sink::UnionReceiveSink;
