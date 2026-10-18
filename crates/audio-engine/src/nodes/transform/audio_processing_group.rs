use std::sync::Arc;

use crate::{
    audio::{
        engine::AudioProcessor,
        format::AudioBlock,
        graph::{GroupProcessorSpec, TransformFailurePolicy},
    },
    error::Result,
    plugins::instance::PluginProcessor,
};

/// Runtime adapter for a persisted serial group. The graph sees one processor;
/// its members keep their normal plugin instances and lifecycle.
pub struct AudioProcessingGroupProcessor {
    processors: Vec<(GroupProcessorSpec, Arc<PluginProcessor>)>,
}

impl AudioProcessingGroupProcessor {
    pub fn new(processors: Vec<(GroupProcessorSpec, Arc<PluginProcessor>)>) -> Arc<Self> {
        Arc::new(Self { processors })
    }
}

impl AudioProcessor for AudioProcessingGroupProcessor {
    fn process_in_place(&self, block: &mut AudioBlock) -> Result<()> {
        for (member, processor) in &self.processors {
            let input = block.clone();
            if let Err(error) = processor.process_in_place(block) {
                match member.failure_policy {
                    TransformFailurePolicy::Bypass => *block = input,
                    TransformFailurePolicy::Silence | TransformFailurePolicy::StopNode => {
                        block.clear()
                    }
                }
                return Err(error);
            }
            block.apply_gain_db(member.gain_db);
            if member.mute {
                block.clear();
            }
        }
        Ok(())
    }

    fn latency_frames(&self) -> u32 {
        self.processors.iter().fold(0u32, |total, (_, processor)| {
            total.saturating_add(processor.latency_frames())
        })
    }

    fn queued_frames(&self) -> u32 {
        self.processors.iter().fold(0u32, |total, (_, processor)| {
            total.saturating_add(processor.queued_frames())
        })
    }
}
