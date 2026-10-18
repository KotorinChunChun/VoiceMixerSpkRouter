use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        Arc,
    },
};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    audio::{engine::AudioProcessor, format::AudioBlock, graph::SwitchPolicy},
    error::Result,
    plugins::process_protocol::PluginStats,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PluginState {
    Created,
    Ready,
    Running,
    Bypassed,
    Recovering,
    Error,
    Stopped,
}

impl PluginState {
    fn as_u8(self) -> u8 {
        match self {
            Self::Created => 0,
            Self::Ready => 1,
            Self::Running => 2,
            Self::Bypassed => 3,
            Self::Recovering => 4,
            Self::Error => 5,
            Self::Stopped => 6,
        }
    }
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Ready,
            2 => Self::Running,
            3 => Self::Bypassed,
            4 => Self::Recovering,
            5 => Self::Error,
            6 => Self::Stopped,
            _ => Self::Created,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PluginPrepareContext {
    pub sample_rate: u32,
    pub channels: u16,
    pub graph_block_frames: usize,
}

pub trait TransformInstance: Send {
    fn prepare(&mut self, context: PluginPrepareContext) -> Result<()>;
    fn start(&mut self) -> Result<()> {
        Ok(())
    }
    fn process(&mut self, block: &mut AudioBlock) -> Result<()>;
    fn process_ports(
        &mut self,
        inputs: &mut [AudioBlock],
        outputs: &mut [AudioBlock],
    ) -> Result<()> {
        if outputs.is_empty() {
            return Ok(());
        }
        if let Some(input) = inputs.first() {
            outputs[0].samples.copy_from_slice(&input.samples);
            outputs[0].copy_latency_from(input);
        } else {
            outputs[0].clear();
        }
        self.process(&mut outputs[0])
    }
    fn stop(&mut self) -> Result<()> {
        Ok(())
    }
    fn set_parameter(&mut self, id: &str, value: Value) -> Result<()>;
    fn set_connected_ports(&mut self, _connected: &[String]) -> Result<()> {
        Ok(())
    }
    fn latency_frames(&self) -> u32 {
        0
    }
    /// Dynamic latency reported by the plugin protocol. `Some(0)` is a real
    /// report and therefore takes precedence over manifest and implementation
    /// defaults; `None` means that no runtime value has been received.
    fn latency_override(&self) -> Option<u32> {
        None
    }
    fn queued_frames(&self) -> u32 {
        0
    }
    fn latest_stats(&self) -> Option<PluginStats> {
        None
    }
}

pub struct PluginProcessor {
    inner: Mutex<Box<dyn TransformInstance>>,
    bypass: AtomicBool,
    switching: AtomicBool,
    switch_policy: AtomicU8,
    state: AtomicU8,
    realtime_busy_bypass: AtomicBool,
    parameters: Mutex<BTreeMap<String, Value>>,
    connected_ports: Mutex<Option<Vec<String>>>,
    last_error: Mutex<Option<String>>,
    latest_stats: Mutex<Option<PluginStats>>,
    manifest_latency_frames: u32,
}

impl PluginProcessor {
    pub fn new(
        instance: Box<dyn TransformInstance>,
        parameters: BTreeMap<String, Value>,
    ) -> Arc<Self> {
        Self::new_with_manifest_latency(instance, parameters, 0)
    }

    pub fn new_with_manifest_latency(
        instance: Box<dyn TransformInstance>,
        parameters: BTreeMap<String, Value>,
        manifest_latency_frames: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(instance),
            bypass: AtomicBool::new(false),
            switching: AtomicBool::new(false),
            switch_policy: AtomicU8::new(SwitchPolicy::Silence.as_u8()),
            state: AtomicU8::new(PluginState::Created.as_u8()),
            realtime_busy_bypass: AtomicBool::new(false),
            parameters: Mutex::new(parameters),
            connected_ports: Mutex::new(None),
            last_error: Mutex::new(None),
            latest_stats: Mutex::new(None),
            manifest_latency_frames: manifest_latency_frames.min(u32::MAX as usize) as u32,
        })
    }

    pub fn prepare(&self, context: PluginPrepareContext) -> Result<()> {
        self.inner.lock().prepare(context)?;
        self.state
            .store(PluginState::Ready.as_u8(), Ordering::Release);
        Ok(())
    }

    pub fn start(&self) -> Result<()> {
        self.inner.lock().start()?;
        self.state
            .store(PluginState::Running.as_u8(), Ordering::Release);
        Ok(())
    }

    pub fn stop(&self) -> Result<()> {
        self.inner.lock().stop()?;
        self.state
            .store(PluginState::Stopped.as_u8(), Ordering::Release);
        Ok(())
    }

    pub fn set_bypass(&self, bypass: bool) {
        self.bypass.store(bypass, Ordering::Release);
        self.state.store(
            if bypass {
                PluginState::Bypassed
            } else {
                PluginState::Ready
            }
            .as_u8(),
            Ordering::Release,
        );
    }

    pub fn is_bypassed(&self) -> bool {
        self.bypass.load(Ordering::Acquire)
    }
    pub fn state(&self) -> PluginState {
        PluginState::from_u8(self.state.load(Ordering::Acquire))
    }

    /// 短い制御操作中に入力通過を許すのはbuiltin DSPだけにする。
    /// 外部pluginはRVCなどの変換前音声を漏らさないよう無音を維持する。
    pub fn set_realtime_busy_bypass(&self, enabled: bool) {
        self.realtime_busy_bypass.store(enabled, Ordering::Release);
    }
    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().clone()
    }
    pub fn parameters(&self) -> BTreeMap<String, Value> {
        self.parameters.lock().clone()
    }

    pub fn set_parameter(&self, id: &str, value: Value) -> Result<()> {
        self.inner.lock().set_parameter(id, value.clone())?;
        self.parameters.lock().insert(id.to_string(), value);
        Ok(())
    }

    /// プリセット・復帰を音声処理の同じロックでまとめて反映する。
    pub fn set_parameters_batch(&self, values: &BTreeMap<String, Value>) -> Result<()> {
        let mut inner = self.inner.lock();
        let mut stored = self.parameters.lock();
        let previous = stored.clone();
        for (id, value) in values {
            if let Err(error) = inner.set_parameter(id, value.clone()) {
                for (old_id, old_value) in &previous {
                    if inner.set_parameter(old_id, old_value.clone()).is_err() {
                        let _ = inner.stop();
                        return Err(crate::error::BackendError::InvalidOperation(format!(
                            "設定失敗後の復帰にも失敗したため処理を停止しました: {error}"
                        )));
                    }
                }
                return Err(error);
            }
        }
        stored.extend(values.clone());
        Ok(())
    }

    pub fn set_connected_ports(&self, connected: &[String]) -> Result<()> {
        // 接続が不変なら、背景のモデル解放が保持するinnerを待つ必要はない。
        // cache単独のガードをここで解放し、以降のロック順はinner→cacheを守る。
        if self.connected_ports.lock().as_deref() == Some(connected) {
            return Ok(());
        }
        // Graphのパラメーター更新でも実行計画は再コンパイルされる。接続が
        // 同じなら外部pluginへPorts制御メッセージを再送しない。RVCではこの
        // 再送がノード移動・パラメーター変更時の不要なエラー源になっていた。
        // replace_instanceと同じくinner、補助cacheの順に取得する。
        // 変更時はinner取得後にも再確認する。
        let mut inner = self.inner.lock();
        let mut cached = self.connected_ports.lock();
        if cached.as_deref() == Some(connected) {
            return Ok(());
        }
        inner.set_connected_ports(connected)?;
        *cached = Some(connected.to_vec());
        Ok(())
    }

    pub fn set_switch_policy(&self, policy: SwitchPolicy) {
        self.switch_policy.store(policy.as_u8(), Ordering::Release);
    }

    pub fn switch_policy(&self) -> SwitchPolicy {
        SwitchPolicy::from_u8(self.switch_policy.load(Ordering::Acquire))
    }

    pub fn is_switching(&self) -> bool {
        self.switching.load(Ordering::Acquire)
    }

    pub fn begin_switch(&self) {
        self.switching.store(true, Ordering::Release);
        self.state
            .store(PluginState::Recovering.as_u8(), Ordering::Release);
    }

    pub fn end_switch(&self) {
        self.switching.store(false, Ordering::Release);
        if !self.bypass.load(Ordering::Acquire) {
            self.state
                .store(PluginState::Running.as_u8(), Ordering::Release);
        }
    }

    pub fn replace_instance(
        &self,
        instance: Box<dyn TransformInstance>,
        parameters: BTreeMap<String, Value>,
        prepare: PluginPrepareContext,
    ) -> Result<()> {
        let mut inner = self.inner.lock();
        let _ = inner.stop();
        *inner = instance;
        *self.parameters.lock() = parameters;
        *self.connected_ports.lock() = None;
        inner.prepare(prepare)?;
        inner.start()?;
        Ok(())
    }

    pub fn cached_stats(&self) -> Option<PluginStats> {
        self.latest_stats.lock().clone()
    }

    pub fn refresh_stats(&self) {
        let Some(instance) = self.inner.try_lock() else {
            return;
        };
        if let Some(stats) = instance.latest_stats() {
            *self.latest_stats.lock() = Some(stats);
        }
    }

    fn apply_switch_policy(&self, block: &mut AudioBlock) {
        if self.switch_policy() == SwitchPolicy::Silence {
            block.samples.fill(0.0);
        }
    }
}

#[cfg(test)]
mod connection_tests {
    use super::*;
    use std::{sync::mpsc, thread, time::Duration};

    #[test]
    fn unchanged_connections_do_not_wait_for_background_model_work() {
        struct TestInstance;
        impl TransformInstance for TestInstance {
            fn prepare(&mut self, _: PluginPrepareContext) -> Result<()> {
                Ok(())
            }
            fn process(&mut self, _: &mut AudioBlock) -> Result<()> {
                Ok(())
            }
            fn set_parameter(&mut self, _: &str, _: Value) -> Result<()> {
                Ok(())
            }
        }
        let processor = PluginProcessor::new(Box::new(TestInstance), BTreeMap::new());
        processor.set_connected_ports(&["audio_in".into()]).unwrap();
        let (entered, ready) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let preparing = processor.clone();
        let worker = thread::spawn(move || {
            let _busy = preparing.inner.lock();
            entered.send(()).unwrap();
            wait.recv().unwrap();
        });
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        let (complete, completed) = mpsc::channel();
        let audio = processor.clone();
        let callback = thread::spawn(move || {
            complete
                .send(audio.set_connected_ports(&["audio_in".into()]))
                .unwrap();
        });
        let result = completed.recv_timeout(Duration::from_secs(1));
        // 回帰があっても試験のworkerは必ず解放する。
        release.send(()).unwrap();
        worker.join().unwrap();
        callback.join().unwrap();
        result
            .expect("接続不変の通知が背景のモデル処理を待った")
            .unwrap();
    }
}

impl AudioProcessor for PluginProcessor {
    fn set_connected_ports(&self, connected: &[String]) -> Result<()> {
        PluginProcessor::set_connected_ports(self, connected)
    }

    fn process_in_place(&self, block: &mut AudioBlock) -> Result<()> {
        if self.bypass.load(Ordering::Acquire) {
            self.state
                .store(PluginState::Bypassed.as_u8(), Ordering::Release);
            return Ok(());
        }
        if self.switching.load(Ordering::Acquire) {
            self.apply_switch_policy(block);
            return Ok(());
        }
        let Some(mut instance) = self.inner.try_lock() else {
            if self.switching.load(Ordering::Acquire) {
                self.apply_switch_policy(block);
                return Ok(());
            }
            // Ports通知・再準備などの制御操作中でも、realtime threadは待たない。
            // 外部pluginは生音漏洩を防ぐため無音、builtinだけ入力を通過させる。
            if !self.realtime_busy_bypass.load(Ordering::Acquire) {
                block.clear();
            }
            return Ok(());
        };
        match instance.process(block) {
            Ok(()) => {
                if let Some(stats) = instance.latest_stats() {
                    if let Some(mut slot) = self.latest_stats.try_lock() {
                        *slot = Some(stats);
                    }
                }
                self.state
                    .store(PluginState::Running.as_u8(), Ordering::Release);
                Ok(())
            }
            Err(err) => {
                self.state
                    .store(PluginState::Error.as_u8(), Ordering::Release);
                if let Some(mut slot) = self.last_error.try_lock() {
                    *slot = Some(err.to_string());
                }
                Err(err)
            }
        }
    }

    fn latency_frames(&self) -> u32 {
        self.inner.try_lock().map_or(0, |instance| {
            select_plugin_latency_frames(
                instance.latency_override(),
                self.manifest_latency_frames,
                instance.latency_frames(),
            )
        })
    }

    fn process_ports(&self, inputs: &mut [AudioBlock], outputs: &mut [AudioBlock]) -> Result<()> {
        if outputs.is_empty() {
            return Ok(());
        }
        if self.bypass.load(Ordering::Acquire) {
            for (output, input) in outputs.iter_mut().zip(inputs.iter()) {
                output.samples.copy_from_slice(&input.samples);
                output.copy_latency_from(input);
            }
            return Ok(());
        }
        if self.switching.load(Ordering::Acquire) {
            self.apply_switch_policy(&mut outputs[0]);
            for output in outputs.iter_mut().skip(1) {
                output.clear();
            }
            return Ok(());
        }
        let Some(mut instance) = self.inner.try_lock() else {
            // 外部pluginが制御中なら生音を漏らさずclearする。builtin DSPのみ
            // main inputを通過させ、sidechainなど追加outputは常にclearする。
            if self.realtime_busy_bypass.load(Ordering::Acquire) {
                if let Some(input) = inputs.first() {
                    outputs[0].samples.copy_from_slice(&input.samples);
                    outputs[0].copy_latency_from(input);
                } else {
                    outputs[0].clear();
                }
            } else {
                outputs[0].clear();
            }
            for output in outputs.iter_mut().skip(1) {
                output.clear();
            }
            return Ok(());
        };
        instance.process_ports(inputs, outputs)
    }

    fn queued_frames(&self) -> u32 {
        self.inner
            .try_lock()
            .map(|x| x.queued_frames())
            .unwrap_or(0)
    }
}

/// Select the effective plugin latency without double-counting values that
/// originate from different reporting layers. Runtime protocol reports take
/// precedence over the static manifest; the implementation's own value is a
/// compatibility fallback for built-in/v1 plugins.
pub fn select_plugin_latency_frames(
    runtime_latency_frames: Option<u32>,
    manifest_latency_frames: u32,
    implementation_latency_frames: u32,
) -> u32 {
    runtime_latency_frames
        .or_else(|| (manifest_latency_frames > 0).then_some(manifest_latency_frames))
        .unwrap_or(implementation_latency_frames)
}

#[cfg(test)]
mod latency_tests {
    use super::select_plugin_latency_frames;

    #[test]
    fn runtime_report_wins_over_manifest_and_fallback() {
        assert_eq!(select_plugin_latency_frames(Some(12), 96, 48), 12);
    }

    #[test]
    fn explicit_runtime_zero_is_not_replaced_by_manifest() {
        assert_eq!(select_plugin_latency_frames(Some(0), 96, 48), 0);
    }

    #[test]
    fn manifest_is_used_when_runtime_is_unknown() {
        assert_eq!(select_plugin_latency_frames(None, 96, 48), 96);
        assert_eq!(select_plugin_latency_frames(None, 0, 48), 48);
    }
}
