// Copyright 2022 Datafuse Labs.
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

use std::any::Any;
use std::sync::Arc;

use common_base::base::Progress;
use common_base::base::ProgressValues;
use common_catalog::table_context::TableContext;
use common_exception::Result;
use common_expression::DataBlock;
use common_pipeline_core::processors::port::OutputPort;
use common_pipeline_core::processors::processor::Event;
use common_pipeline_core::processors::processor::ProcessorPtr;
use common_pipeline_core::processors::Processor;

#[async_trait::async_trait]
pub trait AsyncSource: Send {
    const NAME: &'static str;
    const SKIP_EMPTY_DATA_BLOCK: bool = true;

    #[async_trait::unboxed_simple]
    async fn generate(&mut self) -> Result<Option<DataBlock>>;
}

// TODO: This can be refactored using proc macros
// TODO: Most of its current code is consistent with sync. We need refactor this with better async
// scheduling after supported expand processors. It will be implemented using a similar dynamic window.
pub struct AsyncSourcer<T: 'static + AsyncSource> {
    is_finish: bool,

    inner: T,
    output: Arc<OutputPort>,
    scan_progress: Arc<Progress>,
    generated_data: Option<DataBlock>,
}

impl<T: 'static + AsyncSource> AsyncSourcer<T> {
    pub fn create(
        ctx: Arc<dyn TableContext>,
        output: Arc<OutputPort>,
        inner: T,
    ) -> Result<ProcessorPtr> {
        let scan_progress = ctx.get_scan_progress();
        Ok(ProcessorPtr::create(Box::new(Self {
            inner,
            output,
            scan_progress,
            is_finish: false,
            generated_data: None,
        })))
    }
}

#[async_trait::async_trait]
impl<T: 'static + AsyncSource> Processor for AsyncSourcer<T> {
    fn name(&self) -> String {
        T::NAME.to_string()
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }

    fn event(&mut self) -> Result<Event> {
        if self.is_finish {
            self.output.finish();
            return Ok(Event::Finished);
        }

        if self.output.is_finished() {
            return Ok(Event::Finished);
        }

        if !self.output.can_push() {
            return Ok(Event::NeedConsume);
        }

        match self.generated_data.take() {
            None => Ok(Event::Async),
            Some(data_block) => {
                self.output.push_data(Ok(data_block));
                Ok(Event::NeedConsume)
            }
        }
    }

    #[async_backtrace::framed]
    async fn async_process(&mut self) -> Result<()> {
        match self.inner.generate().await? {
            None => self.is_finish = true,
            Some(data_block) => {
                if !data_block.is_empty() {
                    let progress_values = ProgressValues {
                        rows: data_block.num_rows(),
                        bytes: data_block.memory_size(),
                    };
                    self.scan_progress.incr(&progress_values);
                }

                if !T::SKIP_EMPTY_DATA_BLOCK || !data_block.is_empty() {
                    self.generated_data = Some(data_block)
                }
            }
        };

        Ok(())
    }
}
