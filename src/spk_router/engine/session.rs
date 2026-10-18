use super::{
    browser::{reset_browser_buffer, BrowserReceiver},
    routing::{configure_graph, project, resolve_route, Route},
};
use crate::{
    config::{write_json, Mode, Settings},
    state::{Phase, Status},
    telemetry::{Collector, Snapshot},
};
use audio_mixer_backend::{
    audio::graph::{NodeId, RuntimeState},
    edge::receiver::EdgeStreamState,
    standalone::StandaloneSession,
    windows::worker_status::DeviceWorkerState,
};
use std::{
    path::Path,
    time::{Duration, Instant},
};

const DEVICE_READY_TIMEOUT: Duration = Duration::from_secs(3);
const BROWSER_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(3);

pub(super) struct Resident {
    session: StandaloneSession,
    input: NodeId,
    outputs: [NodeId; 2],
    route: Option<Route>,
    browser: BrowserReceiver,
    meters: Collector,
}

impl Resident {
    pub(super) fn new(settings: &Settings, directory: &Path) -> Result<Self, String> {
        settings.validate()?;
        let (config, input, outputs) = project(settings);
        let path = directory.join("spk-router-project.json");
        write_json(&path, &config)?;
        let mut session =
            StandaloneSession::load(&path, None).map_err(|error| error.to_string())?;
        session.start().map_err(|error| error.to_string())?;
        Ok(Self {
            session,
            input,
            outputs,
            route: None,
            browser: BrowserReceiver::default(),
            meters: Collector::new(input, outputs),
        })
    }

    pub(super) fn is_configured(&self) -> bool {
        self.route.is_some()
    }
    pub(super) fn silence(&mut self) {
        self.meters.reset();
        self.meters.set_active(false, [false; 2]);
        let backend = self.session.backend();
        let mut graph = backend.controller.snapshot();
        for node in graph.nodes.values_mut() {
            node.properties.set_mute(true);
            node.runtime_enabled = false;
        }
        let _ = backend.controller.replace_graph(graph);
        // 出力を先に解放し、旧入力・出力バッファが次のモードへ残らないようにする。
        for id in self.outputs.into_iter().chain([self.input]) {
            let _ = backend.nodes.stop_node(id);
        }
        reset_browser_buffer(
            &backend.edge,
            self.route.as_ref().and_then(|route| route.input.as_ref()),
        );
        self.route = None;
    }

    pub(super) fn configure(&mut self, settings: &Settings) -> Result<(), String> {
        let result = self.configure_inner(settings);
        if result.is_err() {
            self.silence();
        }
        result
    }

    fn configure_inner(&mut self, settings: &Settings) -> Result<(), String> {
        let desired = resolve_route(settings)?;
        self.configure_browser(settings)?;
        self.apply_route(desired)
    }

    fn configure_browser(&mut self, settings: &Settings) -> Result<(), String> {
        if !settings.input_enabled[2] {
            self.browser.disable();
            return Ok(());
        }
        if self.browser.needs_restart(settings) {
            if settings.mode == Mode::Browser {
                self.silence();
            }
            let hub = self.session.backend().edge.clone();
            if let Err(error) = self.browser.configure(settings, hub) {
                if settings.mode == Mode::Browser {
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    fn apply_route(&mut self, desired: Route) -> Result<(), String> {
        if self.route.as_ref() == Some(&desired) {
            return Ok(());
        }
        self.meters.reset();
        let source_changed = self.route.as_ref().map(|route| &route.input) != Some(&desired.input);
        if source_changed {
            self.silence();
        }
        let backend = self.session.backend();
        let previous_outputs = self
            .route
            .as_ref()
            .map(|route| route.outputs.clone())
            .unwrap_or([None, None]);
        // 出力だけの切替は変更されたMonitorだけを停止する。他方と取得元は維持する。
        for (index, id) in self.outputs.into_iter().enumerate() {
            if previous_outputs[index] != desired.outputs[index] {
                backend
                    .nodes
                    .stop_node(id)
                    .map_err(|error| error.to_string())?;
            }
        }
        let mut graph = backend.controller.snapshot();
        configure_graph(&mut graph, self.input, self.outputs, &desired);
        if source_changed {
            for node in graph.nodes.values_mut() {
                node.properties.set_mute(true);
            }
        }
        backend
            .controller
            .replace_graph(graph)
            .map_err(|error| error.to_string())?;
        for (index, id) in self.outputs.into_iter().enumerate() {
            if desired.outputs[index].is_some() && !backend.nodes.is_active(id) {
                backend
                    .nodes
                    .start_node(id)
                    .map_err(|error| error.to_string())?;
            }
        }
        if desired.input.is_some() && !backend.nodes.is_active(self.input) {
            // ReceiverをOFFにしている間もブラウザからPCMは到着するため、再選択前に捨てる。
            reset_browser_buffer(&backend.edge, desired.input.as_ref());
            backend
                .nodes
                .start_node(self.input)
                .map_err(|error| error.to_string())?;
        }
        backend
            .nodes
            .wait_for_device_workers(DEVICE_READY_TIMEOUT)
            .map_err(|error| error.to_string())?;
        if source_changed {
            let mut graph = backend.controller.snapshot();
            configure_graph(&mut graph, self.input, self.outputs, &desired);
            backend
                .controller
                .replace_graph(graph)
                .map_err(|error| error.to_string())?;
        }
        self.meters.set_active(
            desired.input.is_some(),
            desired.outputs.each_ref().map(Option::is_some),
        );
        self.route = Some(desired);
        Ok(())
    }

    pub(super) fn tick(&mut self, settings: &Settings) -> Result<Status, String> {
        let meters = &mut self.meters;
        self.session
            .tick_with_runtime_event_policy(|event| {
                meters.observe(event);
                false
            })
            .map_err(|error| error.to_string())?;
        self.browser.poll();
        if settings.mode == Mode::Browser {
            if let Some(error) = self.browser.error() {
                return Err(error.into());
            }
        }
        if settings.mode == Mode::Stopped {
            return Ok(self.with_browser_status(Status::new(
                settings.mode,
                Phase::Stopped,
                "停止",
            )));
        }
        let backend = self.session.backend();
        for id in self.outputs.into_iter().chain([self.input]) {
            if let Some(worker) = backend.nodes.device_worker_status(id) {
                if worker.state != DeviceWorkerState::Running {
                    return Err(format!(
                        "音声デバイスが切断されました: {}",
                        worker
                            .last_error
                            .unwrap_or_else(|| "再接続を待っています".into())
                    ));
                }
            }
        }
        let diagnostics = backend.nodes.runtime_diagnostics(self.input);
        if let Some(error) = diagnostics.last_error.or_else(|| {
            diagnostics
                .composite
                .and_then(|composite| composite.last_error)
        }) {
            return Err(format!("音声取得に失敗しました: {error}"));
        }
        let graph = backend.controller.snapshot();
        let input = &graph.nodes[&self.input];
        let waiting = if settings.mode == Mode::Browser {
            !backend
                .edge
                .get(&settings.browser_stream_key)
                .is_some_and(|stream| {
                    stream.state(BROWSER_HEARTBEAT_TIMEOUT) == EdgeStreamState::Running
                })
        } else {
            matches!(
                input.state,
                RuntimeState::Waiting
                    | RuntimeState::Disconnected
                    | RuntimeState::Stopped
                    | RuntimeState::Starting
            )
        };
        Ok(self.with_browser_status(if waiting {
            Status::new(
                settings.mode,
                Phase::Waiting,
                format!("{}の音声待ち", settings.mode.label()),
            )
        } else {
            Status::new(
                settings.mode,
                Phase::Running,
                if settings.outputs.iter().any(|output| output.enabled) {
                    format!("{}を転送中", settings.mode.label())
                } else {
                    format!("{}を取得中（出力OFF）", settings.mode.label())
                },
            )
        }))
    }

    fn with_browser_status(&self, mut status: Status) -> Status {
        if let Some(error) = self.browser.error() {
            status.message.push_str(" / ");
            status.message.push_str(error);
        }
        status
    }

    pub(super) fn meter_snapshot(&mut self, mode: Mode) -> Option<Snapshot> {
        let mut value = self.meters.publish(Instant::now(), 0, mode)?;
        let output_active = self
            .route
            .as_ref()
            .map(|r| r.outputs.each_ref().map(Option::is_some))
            .unwrap_or([false; 2]);
        for sample in &mut value.samples {
            if mode == Mode::Stopped {
                sample.input = Some(0.0);
            }
            for (index, active) in output_active.iter().enumerate() {
                if mode == Mode::Stopped || !active {
                    sample.outputs[index] = Some(0.0);
                }
            }
        }
        Some(value)
    }

    pub(super) fn discard_events(&mut self) {
        let _ = self.session.tick();
    }
}

impl Drop for Resident {
    fn drop(&mut self) {
        self.silence();
        self.browser.stop();
        let _ = self.session.stop();
    }
}

#[cfg(all(test, feature = "edge-websocket"))]
mod meter_tests {
    use super::*;

    #[test]
    fn browser_pcm_moves_input_meter_with_outputs_off_and_stop_releases_capture() {
        let directory = tempfile::tempdir().unwrap();
        let port = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut settings = Settings::default();
        settings.browser_port = port.local_addr().unwrap().port();
        settings.mode = Mode::Browser;
        drop(port);
        let mut resident = Resident::new(&settings, directory.path()).unwrap();
        resident.configure(&settings).unwrap();
        let backend = resident.session.backend().clone();
        let _lease = backend
            .edge
            .acquire_pcm_lease(settings.browser_stream_key.clone(), 48_000, 2)
            .unwrap();
        let stream = backend.edge.get(&settings.browser_stream_key).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut observed = false;
        while Instant::now() < deadline {
            stream.jitter.push(&[0.5; 960]);
            resident.tick(&settings).unwrap();
            if let Some(snapshot) = resident.meter_snapshot(settings.mode) {
                assert!(snapshot
                    .samples
                    .iter()
                    .all(|sample| sample.outputs == [Some(0.0); 2]));
                if snapshot
                    .samples
                    .iter()
                    .any(|sample| sample.input.is_some_and(|peak| peak > 0.4))
                {
                    observed = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(observed, "実音声イベントのINピークを観測できない");
        settings.mode = Mode::Stopped;
        resident.configure(&settings).unwrap();
        assert!(!backend.nodes.is_active(resident.input));
        std::thread::sleep(Duration::from_millis(60));
        resident.tick(&settings).unwrap();
        let stopped = resident.meter_snapshot(Mode::Stopped).unwrap();
        assert!(stopped
            .samples
            .iter()
            .all(|sample| sample.input == Some(0.0) && sample.outputs == [Some(0.0); 2]));
    }
}

#[cfg(all(test, feature = "edge-websocket"))]
mod tests {
    use super::*;

    #[test]
    fn resident_keeps_backend_and_browser_listener_across_stop_and_resume() {
        let directory = tempfile::tempdir().unwrap();
        let free_port = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut settings = Settings::default();
        settings.browser_port = free_port.local_addr().unwrap().port();
        drop(free_port);
        let mut resident = Resident::new(&settings, directory.path()).unwrap();
        resident.configure(&settings).unwrap();
        let backend = resident.session.backend().clone();
        assert_eq!(resident.tick(&settings).unwrap().phase, Phase::Stopped);
        assert!(backend
            .controller
            .list_nodes()
            .iter()
            .all(|node| !backend.nodes.is_active(node.id)));

        settings.mode = Mode::Browser;
        resident.configure(&settings).unwrap();
        assert_eq!(resident.tick(&settings).unwrap().phase, Phase::Waiting);
        assert!(backend.nodes.is_active(resident.input));
        assert_eq!(
            backend.edge.receiver_state(&settings.browser_stream_key),
            audio_mixer_backend::edge::control::EdgeCaptureState::Running
        );
        let stream = backend.edge.get(&settings.browser_stream_key).unwrap();
        let lease = backend
            .edge
            .acquire_pcm_lease(settings.browser_stream_key.clone(), 48_000, 2)
            .unwrap();
        assert_eq!(resident.tick(&settings).unwrap().phase, Phase::Running);

        settings.mode = Mode::Stopped;
        resident.configure(&settings).unwrap();
        assert!(std::sync::Arc::ptr_eq(&backend, resident.session.backend()));
        assert!(!backend.nodes.is_active(resident.input));
        assert_eq!(
            backend.edge.receiver_state(&settings.browser_stream_key),
            audio_mixer_backend::edge::control::EdgeCaptureState::Stopped
        );
        assert!(std::net::TcpListener::bind((
            std::net::Ipv4Addr::LOCALHOST,
            settings.browser_port
        ))
        .is_err());
        stream.jitter.push(&vec![0.5; 48_000]);

        settings.mode = Mode::Browser;
        resident.configure(&settings).unwrap();
        assert!(std::sync::Arc::ptr_eq(&backend, resident.session.backend()));
        assert!(std::sync::Arc::ptr_eq(
            &stream,
            &backend.edge.get(&settings.browser_stream_key).unwrap()
        ));
        assert_eq!(resident.tick(&settings).unwrap().phase, Phase::Running);
        let mut samples = [1.0; 480];
        assert_eq!(stream.jitter.read_into(&mut samples), 0);
        drop(lease);
        settings.mode = Mode::Stopped;
        settings.input_enabled[2] = false;
        resident.configure(&settings).unwrap();
        assert!(
            std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, settings.browser_port))
                .is_ok(),
            "使用OFFではブラウザの待受も解放する"
        );
        settings.input_enabled[2] = true;
        resident.configure(&settings).unwrap();
        assert!(
            std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, settings.browser_port))
                .is_err(),
            "使用ONへ戻すと停止中の待受も復旧する"
        );
        drop(resident);
        assert!(std::net::TcpListener::bind((
            std::net::Ipv4Addr::LOCALHOST,
            settings.browser_port
        ))
        .is_ok());
    }

    #[test]
    fn occupied_browser_port_only_blocks_browser_capture() {
        let directory = tempfile::tempdir().unwrap();
        let occupied = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut settings = Settings::default();
        settings.browser_port = occupied.local_addr().unwrap().port();
        let mut resident = Resident::new(&settings, directory.path()).unwrap();
        resident.configure(&settings).unwrap();
        assert_eq!(resident.tick(&settings).unwrap().phase, Phase::Stopped);
        assert!(resident.browser.error().is_some());

        // 実在しないEXEは探索待ちになり、音声デバイス・実プロセスを起動しない。
        settings.mode = Mode::Application;
        settings.application_path = directory.path().join("not-running-player.exe");
        resident.configure(&settings).unwrap();
        assert!(resident.session.backend().nodes.is_active(resident.input));
        assert_ne!(resident.tick(&settings).unwrap().phase, Phase::Error);
        assert!(resident.browser.error().is_some());

        settings.mode = Mode::Browser;
        assert!(resident
            .configure(&settings)
            .unwrap_err()
            .contains("待受ポート"));
        assert!(resident
            .session
            .backend()
            .controller
            .list_nodes()
            .iter()
            .all(|node| !resident.session.backend().nodes.is_active(node.id)));
        drop(occupied);
        resident.configure(&settings).unwrap();
        assert!(resident.browser.error().is_none());
        assert_eq!(resident.tick(&settings).unwrap().phase, Phase::Waiting);
    }
}
