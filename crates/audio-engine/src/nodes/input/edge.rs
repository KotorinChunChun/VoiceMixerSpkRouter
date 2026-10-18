use crate::audio::{buffer::JitterBuffer, engine::AudioSource, format::AudioBlock};
use crate::error::Result;

#[derive(Clone)]
pub struct EdgeInputSource {
    pub jitter: JitterBuffer,
}
impl AudioSource for EdgeInputSource {
    fn read_into(&self, block: &mut AudioBlock) -> Result<()> {
        block.clear();
        let _ = self.jitter.read_into(&mut block.samples);
        Ok(())
    }
}
