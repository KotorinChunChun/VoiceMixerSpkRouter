//! Out-of-process transform plugin host.
//!
//! Control messages are newline-delimited JSON over the child process stdin /
//! stdout. Audio PCM never travels through that pipe: host->plugin and
//! plugin->host audio use separate memory-mapped SPSC rings. Therefore the
//! realtime `process()` call performs no process I/O and never waits for the
//! child process.

use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, BufWriter, Write},
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    sync::Arc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crossbeam_channel::{unbounded, Receiver};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    audio::format::AudioBlock,
    error::{BackendError, Result},
    plugins::{
        instance::{PluginPrepareContext, TransformInstance},
        manager::PluginFactory,
        manifest::PluginDescriptor,
        package::ProcessPluginLaunch,
        process_protocol::{
            decode_plugin_control_line, HostToPluginMessage, PluginControlLine, PluginStats,
            PluginToHostMessage, SharedAudioTransport, PROCESS_PLUGIN_PROTOCOL_VERSION,
        },
        shared_audio::{SharedAudioRing, SharedAudioRingV2},
    },
};

pub struct ProcessPluginFactory {
    descriptor: PluginDescriptor,
    executable: PathBuf,
    working_directory: PathBuf,
    arguments: Vec<String>,
    start_timeout: Duration,
}

impl ProcessPluginFactory {
    pub fn new(
        descriptor: PluginDescriptor,
        launch: ProcessPluginLaunch,
        package_dir: PathBuf,
    ) -> Self {
        let executable = if launch.executable.is_absolute() {
            launch.executable
        } else {
            package_dir.join(launch.executable)
        };
        Self {
            descriptor,
            executable,
            working_directory: package_dir,
            arguments: launch.arguments,
            start_timeout: Duration::from_millis(launch.start_timeout_ms.max(100)),
        }
    }

    pub fn shared(
        descriptor: PluginDescriptor,
        launch: ProcessPluginLaunch,
        package_dir: PathBuf,
    ) -> Arc<Self> {
        Arc::new(Self::new(descriptor, launch, package_dir))
    }
}

impl PluginFactory for ProcessPluginFactory {
    fn descriptor(&self) -> PluginDescriptor {
        self.descriptor.clone()
    }

    fn create(&self, parameters: &BTreeMap<String, Value>) -> Result<Box<dyn TransformInstance>> {
        if !self.executable.is_file() {
            return Err(BackendError::Plugin(format!(
                "process plugin executable does not exist: {}",
                self.executable.display()
            )));
        }
        Ok(Box::new(ProcessTransformInstance::new(
            self.descriptor.clone(),
            self.executable.clone(),
            self.working_directory.clone(),
            self.arguments.clone(),
            self.start_timeout,
            parameters.clone(),
        )))
    }
}

struct ProcessTransformInstance {
    descriptor: PluginDescriptor,
    executable: PathBuf,
    working_directory: PathBuf,
    arguments: Vec<String>,
    start_timeout: Duration,
    parameters: BTreeMap<String, Value>,
    child: Option<Child>,
    control: Option<BufWriter<ChildStdin>>,
    responses: Option<Receiver<PluginToHostMessage>>,
    reader_thread: Option<JoinHandle<()>>,
    input_ring: Option<SharedAudioRing>,
    output_ring: Option<SharedAudioRing>,
    input_ring_v2: Option<SharedAudioRingV2>,
    output_ring_v2: Option<SharedAudioRingV2>,
    input_port_count: usize,
    output_port_count: usize,
    /// Last latency value received from the process. `Some(0)` is preserved
    /// so an explicit zero report overrides the manifest value.
    latency_frames: Option<u32>,
    prepared: bool,
    started: bool,
    latest_stats: Option<PluginStats>,
}

impl ProcessTransformInstance {
    fn new(
        descriptor: PluginDescriptor,
        executable: PathBuf,
        working_directory: PathBuf,
        arguments: Vec<String>,
        start_timeout: Duration,
        parameters: BTreeMap<String, Value>,
    ) -> Self {
        Self {
            descriptor,
            executable,
            working_directory,
            arguments,
            start_timeout,
            parameters,
            child: None,
            control: None,
            responses: None,
            reader_thread: None,
            input_ring: None,
            output_ring: None,
            input_ring_v2: None,
            output_ring_v2: None,
            input_port_count: 1,
            output_port_count: 1,
            latency_frames: None,
            prepared: false,
            started: false,
            latest_stats: None,
        }
    }

    fn launch(&mut self, context: PluginPrepareContext) -> Result<()> {
        self.shutdown();

        let transport_dir = std::env::temp_dir().join("audio-router-transform-plugins");
        std::fs::create_dir_all(&transport_dir)?;
        let transport_id = Uuid::new_v4().to_string();
        let in_path = transport_dir.join(format!("{transport_id}-host-to-plugin.pcmmap"));
        let out_path = transport_dir.join(format!("{transport_id}-plugin-to-host.pcmmap"));
        let capacity_frames =
            (context.sample_rate as usize * 2).max(context.graph_block_frames * 8);
        let input_port_count = self.descriptor.input_ports.len().max(1);
        let output_port_count = self.descriptor.output_ports.len().max(1);
        let is_v2 = self.descriptor.api_version
            >= crate::plugins::manifest::TRANSFORM_PLUGIN_API_VERSION_V2;
        let (input_ring, output_ring, input_ring_v2, output_ring_v2) = if is_v2 {
            (
                None,
                None,
                Some(SharedAudioRingV2::create(
                    &in_path,
                    input_port_count,
                    capacity_frames,
                    context.sample_rate,
                    context.channels,
                )?),
                Some(SharedAudioRingV2::create(
                    &out_path,
                    output_port_count,
                    capacity_frames,
                    context.sample_rate,
                    context.channels,
                )?),
            )
        } else {
            let capacity_samples = capacity_frames * context.channels as usize;
            (
                Some(SharedAudioRing::create(
                    &in_path,
                    capacity_samples,
                    context.sample_rate,
                    context.channels,
                )?),
                Some(SharedAudioRing::create(
                    &out_path,
                    capacity_samples,
                    context.sample_rate,
                    context.channels,
                )?),
                None,
                None,
            )
        };

        let mut command = Command::new(&self.executable);
        // 常駐アプリから起動してもPython等のコンソールを表示しない。
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }
        let mut child = command
            .args(&self.arguments)
            .current_dir(&self.working_directory)
            .env(
                "AUDIO_ROUTER_PLUGIN_PROTOCOL",
                if is_v2 {
                    crate::plugins::process_protocol::PROCESS_PLUGIN_PROTOCOL_VERSION_V2
                } else {
                    PROCESS_PLUGIN_PROTOCOL_VERSION
                }
                .to_string(),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                BackendError::Plugin(format!("start {}: {e}", self.executable.display()))
            })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| BackendError::Plugin("process plugin stdin is unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| BackendError::Plugin("process plugin stdout is unavailable".into()))?;
        let (tx, rx) = unbounded();
        let reader_thread = thread::Builder::new()
            .name(format!(
                "plugin-control-reader-{}",
                self.descriptor.plugin_id
            ))
            .spawn(move || {
                let reader = BufReader::new(stdout);
                for line in reader.lines() {
                    let Ok(line) = line else {
                        break;
                    };
                    if line.trim().is_empty() {
                        continue;
                    }
                    match decode_plugin_control_line(&line) {
                        PluginControlLine::Message(message) => {
                            if tx.send(message).is_err() {
                                break;
                            }
                        }
                        PluginControlLine::Ignored => {}
                        PluginControlLine::InvalidJson(error) => {
                            let _ = tx.send(PluginToHostMessage::Error {
                                code: "invalid_json".into(),
                                message: error,
                            });
                        }
                    }
                }
            })
            .map_err(BackendError::Io)?;

        self.child = Some(child);
        self.control = Some(BufWriter::new(stdin));
        self.responses = Some(rx);
        self.reader_thread = Some(reader_thread);
        self.output_ring = output_ring;
        self.input_ring = input_ring;
        self.input_ring_v2 = input_ring_v2;
        self.output_ring_v2 = output_ring_v2;
        self.input_port_count = input_port_count;
        self.output_port_count = output_port_count;

        let transport = SharedAudioTransport {
            input_mapping_name: in_path.to_string_lossy().into_owned(),
            output_mapping_name: out_path.to_string_lossy().into_owned(),
            capacity_frames,
            sample_rate: context.sample_rate,
            channels: context.channels,
            layout_version: is_v2
                .then_some(crate::plugins::shared_audio::SHARED_AUDIO_RING_V2_VERSION),
            input_port_mapping_names: if is_v2 {
                vec![in_path.to_string_lossy().into_owned()]
            } else {
                Vec::new()
            },
            output_port_mapping_names: if is_v2 {
                vec![out_path.to_string_lossy().into_owned()]
            } else {
                Vec::new()
            },
        };
        // Model paths and other construction-time parameters must reach the
        // worker before Prepare, because Prepare is where process plugins load
        // models and allocate their processing state.
        if !self.parameters.is_empty() {
            self.send(&HostToPluginMessage::SetParameters(self.parameters.clone()))?;
        }
        self.send(&HostToPluginMessage::Prepare {
            sample_rate: context.sample_rate,
            channels: context.channels,
            graph_block_frames: context.graph_block_frames,
            transport,
        })?;

        let deadline = Instant::now() + self.start_timeout;
        let mut saw_hello = false;
        let expected_protocol = if is_v2 {
            crate::plugins::process_protocol::PROCESS_PLUGIN_PROTOCOL_VERSION_V2
        } else {
            PROCESS_PLUGIN_PROTOCOL_VERSION
        };
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(BackendError::Plugin(format!(
                    "plugin '{}' did not become ready within {} ms",
                    self.descriptor.plugin_id,
                    self.start_timeout.as_millis()
                )));
            }
            let remaining = deadline.saturating_duration_since(now);
            let message = self
                .responses
                .as_ref()
                .unwrap()
                .recv_timeout(remaining)
                .map_err(|_| {
                    BackendError::Plugin(format!(
                        "plugin '{}' control channel closed before Ready",
                        self.descriptor.plugin_id
                    ))
                })?;
            match message {
                PluginToHostMessage::Hello(hello) => {
                    if hello.protocol_version != expected_protocol {
                        return Err(BackendError::Plugin(format!(
                            "plugin '{}' protocol {} does not match host protocol {}",
                            hello.plugin_id, hello.protocol_version, expected_protocol
                        )));
                    }
                    if hello.plugin_id != self.descriptor.plugin_id {
                        return Err(BackendError::Plugin(format!(
                            "launched plugin id '{}' does not match manifest '{}'",
                            hello.plugin_id, self.descriptor.plugin_id
                        )));
                    }
                    saw_hello = true;
                }
                PluginToHostMessage::Ready { latency_frames, .. } => {
                    self.latency_frames = Some(latency_frames);
                    break;
                }
                PluginToHostMessage::Error { code, message } => {
                    return Err(BackendError::Plugin(format!(
                        "plugin prepare error {code}: {message}"
                    )));
                }
                PluginToHostMessage::State { state, message }
                    if state.eq_ignore_ascii_case("error") =>
                {
                    return Err(BackendError::Plugin(
                        message.unwrap_or_else(|| "plugin entered error state".into()),
                    ));
                }
                _ => {}
            }
        }
        if !saw_hello {
            // Ready is sufficient for forward compatibility, but retain a clear
            // diagnostic path for plugins that omit the handshake.
            tracing::warn!(plugin_id=%self.descriptor.plugin_id, "process plugin sent Ready without Hello");
        }
        self.prepared = true;
        Ok(())
    }

    fn send(&mut self, message: &HostToPluginMessage) -> Result<()> {
        let writer = self.control.as_mut().ok_or_else(|| {
            BackendError::Plugin("process plugin control channel is not open".into())
        })?;
        serde_json::to_writer(&mut *writer, message)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        Ok(())
    }

    /// Polls process/control state without waiting. This is safe to call from
    /// the realtime graph path because it never performs blocking process I/O.
    fn poll_runtime_state(&mut self) -> Result<()> {
        if let Some(child) = self.child.as_mut() {
            if let Some(status) = child.try_wait().map_err(BackendError::Io)? {
                return Err(BackendError::Plugin(format!(
                    "plugin '{}' exited unexpectedly with {status}",
                    self.descriptor.plugin_id
                )));
            }
        }
        if let Some(rx) = self.responses.as_ref() {
            loop {
                match rx.try_recv() {
                    Ok(PluginToHostMessage::Error { code, message }) => {
                        return Err(BackendError::Plugin(format!(
                            "plugin runtime error {code}: {message}"
                        )));
                    }
                    Ok(PluginToHostMessage::State { state, message })
                        if state.eq_ignore_ascii_case("error") =>
                    {
                        return Err(BackendError::Plugin(
                            message.unwrap_or_else(|| "plugin entered error state".into()),
                        ));
                    }
                    Ok(PluginToHostMessage::Ready { latency_frames, .. }) => {
                        self.latency_frames = Some(latency_frames);
                    }
                    Ok(PluginToHostMessage::Latency { frames }) => {
                        self.latency_frames = Some(frames);
                    }
                    Ok(PluginToHostMessage::Stats(stats)) => {
                        self.latest_stats = Some(stats);
                    }
                    Ok(_) => {}
                    Err(crossbeam_channel::TryRecvError::Empty) => break,
                    Err(crossbeam_channel::TryRecvError::Disconnected) => {
                        return Err(BackendError::Plugin(format!(
                            "plugin '{}' control channel disconnected",
                            self.descriptor.plugin_id
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    fn shutdown(&mut self) {
        if self.control.is_some() {
            let _ = self.send(&HostToPluginMessage::Stop);
            let _ = self.send(&HostToPluginMessage::Shutdown);
        }
        self.control.take();
        if let Some(mut child) = self.child.take() {
            let deadline = Instant::now() + Duration::from_millis(300);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(10))
                    }
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                }
            }
        }
        self.responses.take();
        if let Some(reader) = self.reader_thread.take() {
            let _ = reader.join();
        }
        self.input_ring.take();
        self.output_ring.take();
        self.input_ring_v2.take();
        self.output_ring_v2.take();
        self.input_port_count = 1;
        self.output_port_count = 1;
        self.prepared = false;
        self.started = false;
    }
}

impl TransformInstance for ProcessTransformInstance {
    fn prepare(&mut self, context: PluginPrepareContext) -> Result<()> {
        self.launch(context)
    }

    fn start(&mut self) -> Result<()> {
        if !self.prepared {
            return Err(BackendError::Plugin(
                "process plugin must be prepared before start".into(),
            ));
        }
        self.send(&HostToPluginMessage::Start)?;
        self.started = true;
        Ok(())
    }

    fn process(&mut self, block: &mut AudioBlock) -> Result<()> {
        if !self.started {
            block.samples.fill(0.0);
            return Ok(());
        }
        self.poll_runtime_state()?;
        let input = self
            .input_ring
            .as_mut()
            .ok_or_else(|| BackendError::Plugin("plugin input transport missing".into()))?;
        let written = input.push(&block.samples);
        let dropped = block.samples.len() - written;

        let output = self
            .output_ring
            .as_mut()
            .ok_or_else(|| BackendError::Plugin("plugin output transport missing".into()))?;
        let read = output.pop(&mut block.samples);
        if read < block.samples.len() {
            block.samples[read..].fill(0.0);
        }
        // 入力が満杯でも出力を回収し、両リングの詰まりを相互に悪化させない。
        if dropped > 0 {
            block.clear();
            return Err(BackendError::Plugin(format!(
                "plugin '{}' input shared ring overflowed by {} samples",
                self.descriptor.plugin_id, dropped
            )));
        }
        Ok(())
    }

    fn process_ports(
        &mut self,
        inputs: &mut [AudioBlock],
        outputs: &mut [AudioBlock],
    ) -> Result<()> {
        if outputs.is_empty() {
            return Ok(());
        }
        if !self.started {
            outputs.iter_mut().for_each(AudioBlock::clear);
            return Ok(());
        }
        self.poll_runtime_state()?;
        if let (Some(input_ring), Some(output_ring)) =
            (self.input_ring_v2.as_mut(), self.output_ring_v2.as_mut())
        {
            let frames = outputs[0].frames();
            let mut overflow = None;
            for port in 0..self.input_port_count {
                let silence;
                let input = if let Some(input) = inputs.get(port) {
                    input
                } else {
                    silence = AudioBlock::silence(frames);
                    &silence
                };
                let written = input_ring.push(port, &input.samples)?;
                if written < input.samples.len() && overflow.is_none() {
                    overflow = Some(BackendError::Plugin(format!(
                        "plugin '{}' input shared ring v2 overflowed on port {}",
                        self.descriptor.plugin_id, port
                    )));
                }
            }
            for port in 0..self.output_port_count.min(outputs.len()) {
                let output = &mut outputs[port];
                let read = output_ring.pop(port, &mut output.samples)?;
                if read < output.samples.len() {
                    output.samples[read..].fill(0.0);
                }
            }
            for output in outputs.iter_mut().skip(self.output_port_count) {
                output.clear();
            }
            if let Some(error) = overflow {
                outputs.iter_mut().for_each(AudioBlock::clear);
                return Err(error);
            }
            return Ok(());
        }

        // v1 remains a single interleaved input/output ring.
        let Some(input) = inputs.first() else {
            outputs[0].clear();
            return Ok(());
        };
        let input_ring = self
            .input_ring
            .as_mut()
            .ok_or_else(|| BackendError::Plugin("plugin input transport missing".into()))?;
        let written = input_ring.push(&input.samples);
        let dropped = input.samples.len() - written;
        let output_ring = self
            .output_ring
            .as_mut()
            .ok_or_else(|| BackendError::Plugin("plugin output transport missing".into()))?;
        let read = output_ring.pop(&mut outputs[0].samples);
        if read < outputs[0].samples.len() {
            outputs[0].samples[read..].fill(0.0);
        }
        if dropped > 0 {
            outputs.iter_mut().for_each(AudioBlock::clear);
            return Err(BackendError::Plugin(format!(
                "plugin '{}' input shared ring overflowed by {} samples",
                self.descriptor.plugin_id, dropped
            )));
        }
        outputs[0].copy_latency_from(input);
        for output in outputs.iter_mut().skip(1) {
            output.clear();
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        self.shutdown();
        Ok(())
    }

    fn set_parameter(&mut self, id: &str, value: Value) -> Result<()> {
        self.parameters.insert(id.to_string(), value.clone());
        if self.control.is_some() {
            let mut update = BTreeMap::new();
            update.insert(id.to_string(), value);
            self.send(&HostToPluginMessage::SetParameters(update))?;
        }
        Ok(())
    }

    fn set_connected_ports(&mut self, connected: &[String]) -> Result<()> {
        if self.control.is_some() {
            self.send(&HostToPluginMessage::Ports {
                connected: connected.to_vec(),
            })?;
        }
        Ok(())
    }

    fn latency_frames(&self) -> u32 {
        self.latency_frames.unwrap_or(0)
    }

    fn latency_override(&self) -> Option<u32> {
        self.latency_frames
    }

    fn queued_frames(&self) -> u32 {
        self.input_ring
            .as_ref()
            .map(|ring| ring.queued_frames() as u32)
            .or_else(|| {
                self.input_ring_v2
                    .as_ref()
                    .and_then(|ring| ring.queued_frames(0).ok().map(|frames| frames as u32))
            })
            .unwrap_or(0)
    }

    fn latest_stats(&self) -> Option<PluginStats> {
        self.latest_stats.clone()
    }
}

impl Drop for ProcessTransformInstance {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_overflow_still_drains_output_and_returns_silent_error() {
        let package = crate::plugins::package::load_package_manifest(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/rvc-cuda/plugin.json"),
        )
        .unwrap();
        for path in ["in_place", "ports_v1", "ports_v2"] {
            let temp = tempfile::tempdir().unwrap();
            let mut instance = ProcessTransformInstance::new(
                package.descriptor.clone(),
                PathBuf::new(),
                temp.path().into(),
                Vec::new(),
                Duration::from_secs(1),
                BTreeMap::new(),
            );
            instance.started = true;
            let input_path = temp.path().join("input.pcmmap");
            let output_path = temp.path().join("output.pcmmap");
            if path == "ports_v2" {
                let mut input = SharedAudioRingV2::create(&input_path, 1, 4, 48000, 2).unwrap();
                let mut output = SharedAudioRingV2::create(&output_path, 1, 4, 48000, 2).unwrap();
                assert_eq!(input.push(0, &[0.5; 8]).unwrap(), 8);
                assert_eq!(output.push(0, &[0.25; 8]).unwrap(), 8);
                instance.input_ring_v2 = Some(input);
                instance.output_ring_v2 = Some(output);
            } else {
                let mut input = SharedAudioRing::create(&input_path, 8, 48000, 2).unwrap();
                let mut output = SharedAudioRing::create(&output_path, 8, 48000, 2).unwrap();
                assert_eq!(input.push(&[0.5; 8]), 8);
                assert_eq!(output.push(&[0.25; 8]), 8);
                instance.input_ring = Some(input);
                instance.output_ring = Some(output);
            }
            let mut input = AudioBlock::from_interleaved_stereo(vec![0.75; 4]);
            let mut outputs = [AudioBlock::silence(2)];
            let error = if path == "in_place" {
                let result = instance.process(&mut input);
                assert!(input.samples.iter().all(|x| *x == 0.0));
                result.unwrap_err()
            } else {
                let result = instance.process_ports(&mut [input], &mut outputs);
                assert!(outputs[0].samples.iter().all(|x| *x == 0.0));
                result.unwrap_err()
            };
            assert!(error.to_string().contains("overflowed"));
            let remaining = if path == "ports_v2" {
                instance
                    .output_ring_v2
                    .as_ref()
                    .unwrap()
                    .queued_frames(0)
                    .unwrap()
            } else {
                instance.output_ring.as_ref().unwrap().queued_frames()
            };
            assert_eq!(remaining, 2, "{path}: エラー時も出力2フレームを回収する");
        }
    }
}
