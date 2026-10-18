use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{
    audio::{engine::AudioProcessor, format::AudioBlock},
    error::{BackendError, Result},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RvcState {
    Uninitialized,
    Ready,
    Running,
    Bypassed,
    Error,
}

pub trait RvcBackend: Send {
    fn initialize(&mut self) -> Result<()>;
    fn load_model(&mut self, model: &str) -> Result<()>;
    fn start(&mut self) -> Result<()>;
    fn process_audio(&mut self, interleaved_stereo_48k: &mut [f32]) -> Result<()>;
    fn stop(&mut self) -> Result<()>;
    fn set_bypass(&mut self, bypass: bool) -> Result<()>;
    fn latency_frames(&self) -> u32;
    fn state(&self) -> RvcState;
}

/// Adapter placed in the Audio Graph. Concrete RVC implementations can be
/// in-process, IPC-based, or FFI-based without changing the graph runtime.
pub struct RvcNodeProcessor {
    backend: Mutex<Box<dyn RvcBackend>>,
}

impl RvcNodeProcessor {
    pub fn new(backend: Box<dyn RvcBackend>) -> Arc<Self> {
        Arc::new(Self {
            backend: Mutex::new(backend),
        })
    }
    pub fn initialize(&self) -> Result<()> {
        self.backend.lock().initialize()
    }
    pub fn load_model(&self, model: &str) -> Result<()> {
        self.backend.lock().load_model(model)
    }
    pub fn start(&self) -> Result<()> {
        self.backend.lock().start()
    }
    pub fn stop(&self) -> Result<()> {
        self.backend.lock().stop()
    }
    pub fn state(&self) -> RvcState {
        self.backend.lock().state()
    }
    pub fn set_bypass(&self, bypass: bool) -> Result<()> {
        self.backend.lock().set_bypass(bypass)
    }
}

impl AudioProcessor for RvcNodeProcessor {
    fn process_in_place(&self, block: &mut AudioBlock) -> Result<()> {
        let Some(mut backend) = self.backend.try_lock() else {
            return Err(BackendError::Rvc(
                "RVC backend is busy; realtime thread did not wait on its mutex".into(),
            ));
        };
        backend
            .process_audio(&mut block.samples)
            .map_err(|e| BackendError::Rvc(e.to_string()))
    }
    fn latency_frames(&self) -> u32 {
        self.backend
            .try_lock()
            .map(|b| b.latency_frames())
            .unwrap_or(0)
    }
}

/// Useful until the actual RVC IPC/API is selected. It preserves audio while
/// exercising graph placement, multiple instances, bypass and latency plumbing.
pub struct PassthroughRvcBackend {
    state: RvcState,
    latency_frames: u32,
    bypass: bool,
}
impl PassthroughRvcBackend {
    pub fn new(latency_frames: u32) -> Self {
        Self {
            state: RvcState::Uninitialized,
            latency_frames,
            bypass: false,
        }
    }
}
impl RvcBackend for PassthroughRvcBackend {
    fn initialize(&mut self) -> Result<()> {
        self.state = RvcState::Ready;
        Ok(())
    }
    fn load_model(&mut self, _model: &str) -> Result<()> {
        self.state = RvcState::Ready;
        Ok(())
    }
    fn start(&mut self) -> Result<()> {
        self.state = RvcState::Running;
        Ok(())
    }
    fn process_audio(&mut self, _audio: &mut [f32]) -> Result<()> {
        if self.bypass {
            self.state = RvcState::Bypassed;
        } else {
            self.state = RvcState::Running;
        }
        Ok(())
    }
    fn stop(&mut self) -> Result<()> {
        self.state = RvcState::Ready;
        Ok(())
    }
    fn set_bypass(&mut self, bypass: bool) -> Result<()> {
        self.bypass = bypass;
        self.state = if bypass {
            RvcState::Bypassed
        } else {
            RvcState::Ready
        };
        Ok(())
    }
    fn latency_frames(&self) -> u32 {
        self.latency_frames
    }
    fn state(&self) -> RvcState {
        self.state
    }
}
