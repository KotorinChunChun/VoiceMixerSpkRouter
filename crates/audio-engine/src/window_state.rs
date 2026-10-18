//! Main-window placement persistence.
//!
//! The persisted coordinates use the same physical coordinate system as
//! Tauri's `outer_position` and `outer_size` APIs.  Keeping the storage and
//! placement logic independent from Tauri makes the safety rules testable
//! without creating a native window.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, ErrorKind, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

pub const WINDOW_STATE_VERSION: u32 = 1;
pub const DEFAULT_WINDOW_WIDTH: u32 = 1600;
pub const DEFAULT_WINDOW_HEIGHT: u32 = 960;
pub const MIN_WINDOW_WIDTH: u32 = 1100;
pub const MIN_WINDOW_HEIGHT: u32 = 700;
pub const MAX_WINDOW_DIMENSION: u32 = 32_768;
pub const WINDOW_STATE_DEBOUNCE_MS: u64 = 350;

const MAX_COORDINATE_MAGNITUDE: i64 = 1_000_000_000;
const LOCK_STALE_AFTER: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowBounds {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl WindowBounds {
    fn validate(&self) -> Result<(), String> {
        if i64::from(self.x).abs() > MAX_COORDINATE_MAGNITUDE
            || i64::from(self.y).abs() > MAX_COORDINATE_MAGNITUDE
        {
            return Err("ウィンドウ座標が許容範囲外です".to_owned());
        }
        if self.width == 0 || self.height == 0 {
            return Err("ウィンドウサイズが0です".to_owned());
        }
        if self.width > MAX_WINDOW_DIMENSION || self.height > MAX_WINDOW_DIMENSION {
            return Err("ウィンドウサイズが許容範囲外です".to_owned());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkArea {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl WorkArea {
    fn validate(&self) -> Result<(), String> {
        if self.width == 0 || self.height == 0 {
            return Err("モニター作業領域のサイズが0です".to_owned());
        }
        if self.width > MAX_WINDOW_DIMENSION || self.height > MAX_WINDOW_DIMENSION {
            return Err("モニター作業領域のサイズが許容範囲外です".to_owned());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MonitorGeometry {
    pub name: Option<String>,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub work_area: WorkArea,
    pub scale_factor: f64,
}

impl MonitorGeometry {
    pub fn is_valid(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.width <= MAX_WINDOW_DIMENSION
            && self.height <= MAX_WINDOW_DIMENSION
            && self.work_area.validate().is_ok()
            && self.scale_factor.is_finite()
            && self.scale_factor > 0.0
    }

    pub fn to_saved(&self) -> SavedMonitor {
        SavedMonitor {
            name: self.name.clone(),
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height,
            work_area: self.work_area,
            scale_factor: self.scale_factor,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SavedMonitor {
    pub name: Option<String>,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub work_area: WorkArea,
    pub scale_factor: f64,
}

impl SavedMonitor {
    fn validate(&self) -> Result<(), String> {
        if self.width == 0 || self.height == 0 {
            return Err("保存済みモニターのサイズが0です".to_owned());
        }
        if self.width > MAX_WINDOW_DIMENSION || self.height > MAX_WINDOW_DIMENSION {
            return Err("保存済みモニターのサイズが許容範囲外です".to_owned());
        }
        self.work_area.validate()?;
        if !self.scale_factor.is_finite() || self.scale_factor <= 0.0 {
            return Err("保存済みモニターのDPI倍率が不正です".to_owned());
        }
        Ok(())
    }

    fn to_geometry(&self) -> MonitorGeometry {
        MonitorGeometry {
            name: self.name.clone(),
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height,
            work_area: self.work_area,
            scale_factor: self.scale_factor,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WindowDisplayState {
    Normal,
    Maximized,
}

impl Default for WindowDisplayState {
    fn default() -> Self {
        Self::Normal
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WindowState {
    pub version: u32,
    pub normal: WindowBounds,
    pub display_state: WindowDisplayState,
    #[serde(default)]
    pub monitor: Option<SavedMonitor>,
}

impl Default for WindowState {
    fn default() -> Self {
        Self {
            version: WINDOW_STATE_VERSION,
            normal: WindowBounds {
                x: 0,
                y: 0,
                width: DEFAULT_WINDOW_WIDTH,
                height: DEFAULT_WINDOW_HEIGHT,
            },
            display_state: WindowDisplayState::Normal,
            monitor: None,
        }
    }
}

impl WindowState {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != WINDOW_STATE_VERSION {
            return Err(format!(
                "ウィンドウ状態のバージョンが未対応です: {}",
                self.version
            ));
        }
        self.normal.validate()?;
        if let Some(monitor) = &self.monitor {
            monitor.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveResult {
    Saved,
    SkippedLocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetResult {
    Reset,
    SkippedLocked,
}

#[derive(Debug, Clone)]
pub struct WindowStateStore {
    path: PathBuf,
    lock_path: PathBuf,
    operation_lock: Arc<Mutex<()>>,
}

impl WindowStateStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let lock_path = path.with_file_name(format!(
            "{}.lock",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("window-state.json")
        ));
        Self {
            path,
            lock_path,
            operation_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn from_app_data_dir(app_data_dir: impl AsRef<Path>) -> Self {
        Self::new(app_data_dir.as_ref().join("window-state.json"))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> io::Result<Option<WindowState>> {
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let state: WindowState = serde_json::from_slice(&bytes).map_err(|error| {
            io::Error::new(
                ErrorKind::InvalidData,
                format!("ウィンドウ状態のJSONを読み込めません: {error}"),
            )
        })?;
        state.validate().map_err(|error| {
            io::Error::new(
                ErrorKind::InvalidData,
                format!("ウィンドウ状態の検証に失敗しました: {error}"),
            )
        })?;
        Ok(Some(state))
    }

    pub fn save(&self, state: &WindowState) -> io::Result<SaveResult> {
        state.validate().map_err(|error| {
            io::Error::new(
                ErrorKind::InvalidInput,
                format!("保存するウィンドウ状態が不正です: {error}"),
            )
        })?;

        if let Some(parent) = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }

        let _operation_guard = self
            .operation_lock
            .lock()
            .map_err(|_| io::Error::other("ウィンドウ状態の保存ロックが壊れています"))?;
        let Some(_file_lock) = self.acquire_file_lock()? else {
            return Ok(SaveResult::SkippedLocked);
        };

        let temp_path = self.temporary_path();
        let result = (|| {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temp_path)?;
            let bytes = serde_json::to_vec_pretty(state).map_err(|error| {
                io::Error::new(
                    ErrorKind::InvalidData,
                    format!("ウィンドウ状態をJSONへ変換できません: {error}"),
                )
            })?;
            file.write_all(&bytes)?;
            file.flush()?;
            file.sync_all()?;
            drop(file);
            atomic_replace(&temp_path, &self.path)
        })();

        if result.is_err() {
            let _ = fs::remove_file(&temp_path);
        }
        result.map(|()| SaveResult::Saved)
    }

    pub fn reset(&self) -> io::Result<ResetResult> {
        if let Some(parent) = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            match fs::metadata(parent) {
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => {
                    return Err(io::Error::new(
                        ErrorKind::InvalidInput,
                        "ウィンドウ状態の保存先がディレクトリではありません",
                    ));
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    return Ok(ResetResult::Reset);
                }
                Err(error) => return Err(error),
            }
        }

        let _operation_guard = self
            .operation_lock
            .lock()
            .map_err(|_| io::Error::other("ウィンドウ状態の保存ロックが壊れています"))?;
        let Some(_file_lock) = self.acquire_file_lock()? else {
            return Ok(ResetResult::SkippedLocked);
        };

        match fs::remove_file(&self.path) {
            Ok(()) => Ok(ResetResult::Reset),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(ResetResult::Reset),
            Err(error) => Err(error),
        }
    }

    fn temporary_path(&self) -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let file_name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("window-state.json");
        self.path.with_file_name(format!(
            ".{file_name}.tmp-{}-{timestamp}",
            std::process::id()
        ))
    }

    fn acquire_file_lock(&self) -> io::Result<Option<FileLockGuard>> {
        for attempt in 0..2 {
            match OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&self.lock_path)
            {
                Ok(mut file) => {
                    if let Err(error) = writeln!(file, "pid={}", std::process::id()) {
                        drop(file);
                        let _ = fs::remove_file(&self.lock_path);
                        return Err(error);
                    }
                    if let Err(error) = file.sync_all() {
                        drop(file);
                        let _ = fs::remove_file(&self.lock_path);
                        return Err(error);
                    }
                    drop(file);
                    return Ok(Some(FileLockGuard {
                        path: self.lock_path.clone(),
                    }));
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    if attempt == 0 && lock_is_stale(&self.lock_path) {
                        match fs::remove_file(&self.lock_path) {
                            Ok(()) => continue,
                            Err(remove_error) if remove_error.kind() == ErrorKind::NotFound => {
                                continue
                            }
                            Err(remove_error) => return Err(remove_error),
                        }
                    }
                    return Ok(None);
                }
                Err(error) => return Err(error),
            }
        }
        Ok(None)
    }
}

struct FileLockGuard {
    path: PathBuf,
}

impl Drop for FileLockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn lock_is_stale(path: &Path) -> bool {
    let Ok(modified) = fs::metadata(path).and_then(|metadata| metadata.modified()) else {
        return false;
    };
    SystemTime::now()
        .duration_since(modified)
        .map(|age| age > LOCK_STALE_AFTER)
        .unwrap_or(false)
}

#[cfg(windows)]
fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    use windows::{
        core::PCWSTR,
        Win32::Storage::FileSystem::{
            MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        },
    };

    let source: Vec<u16> = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        MoveFileExW(
            PCWSTR(source.as_ptr()),
            PCWSTR(destination.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(|error| io::Error::other(format!("ウィンドウ状態を置換できません: {error}")))
}

#[cfg(not(windows))]
fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}

pub fn restore_placement(
    saved_state: Option<&WindowState>,
    monitors: &[MonitorGeometry],
    primary_monitor: Option<&MonitorGeometry>,
) -> (WindowBounds, WindowDisplayState) {
    let valid_monitors: Vec<&MonitorGeometry> = monitors
        .iter()
        .filter(|monitor| monitor.is_valid())
        .collect();

    let valid_primary = primary_monitor.filter(|monitor| monitor.is_valid());
    let target_monitor = saved_state
        .and_then(|state| state.monitor.as_ref())
        .and_then(|saved_monitor| {
            valid_monitors
                .iter()
                .copied()
                .find(|monitor| {
                    saved_monitor
                        .name
                        .as_deref()
                        .zip(monitor.name.as_deref())
                        .is_some_and(|(saved_name, current_name)| saved_name == current_name)
                })
                .or_else(|| {
                    let saved_geometry = saved_monitor.to_geometry();
                    valid_monitors
                        .iter()
                        .copied()
                        .max_by_key(|monitor| overlap_area(&saved_geometry, monitor))
                        .filter(|monitor| overlap_area(&saved_geometry, monitor) > 0)
                })
        })
        .or_else(|| {
            saved_state.and_then(|state| {
                valid_monitors
                    .iter()
                    .copied()
                    .max_by_key(|monitor| overlap_area_with_bounds(&state.normal, monitor))
                    .filter(|monitor| overlap_area_with_bounds(&state.normal, monitor) > 0)
            })
        })
        .or_else(|| {
            saved_state.and_then(|state| {
                valid_monitors
                    .iter()
                    .copied()
                    .min_by_key(|monitor| center_distance_squared(&state.normal, monitor))
            })
        })
        .or(valid_primary)
        .or_else(|| valid_monitors.first().copied());

    let Some(target_monitor) = target_monitor else {
        let bounds = saved_state
            .map(|state| state.normal)
            .unwrap_or_else(default_bounds);
        return (
            clamp_to_default_limits(bounds),
            saved_state
                .map(|state| state.display_state)
                .unwrap_or_default(),
        );
    };

    let bounds = saved_state
        .map(|state| fit_bounds_to_work_area(state.normal, target_monitor.work_area))
        .unwrap_or_else(|| centered_default_bounds(target_monitor.work_area));
    (
        bounds,
        saved_state
            .map(|state| state.display_state)
            .unwrap_or_default(),
    )
}

pub fn fit_bounds_to_work_area(bounds: WindowBounds, work_area: WorkArea) -> WindowBounds {
    let work_width = work_area.width.max(1);
    let work_height = work_area.height.max(1);
    let width = bounds
        .width
        .clamp(MIN_WINDOW_WIDTH, MAX_WINDOW_DIMENSION)
        .min(work_width);
    let height = bounds
        .height
        .clamp(MIN_WINDOW_HEIGHT, MAX_WINDOW_DIMENSION)
        .min(work_height);
    let max_x = i64::from(work_area.x) + i64::from(work_width - width);
    let max_y = i64::from(work_area.y) + i64::from(work_height - height);
    let x = i64::from(bounds.x)
        .clamp(i64::from(work_area.x), max_x)
        .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
    let y = i64::from(bounds.y)
        .clamp(i64::from(work_area.y), max_y)
        .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
    WindowBounds {
        x,
        y,
        width,
        height,
    }
}

fn centered_default_bounds(work_area: WorkArea) -> WindowBounds {
    let width = DEFAULT_WINDOW_WIDTH.min(work_area.width.max(1));
    let height = DEFAULT_WINDOW_HEIGHT.min(work_area.height.max(1));
    WindowBounds {
        x: (i64::from(work_area.x) + i64::from(work_area.width - width) / 2)
            .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
        y: (i64::from(work_area.y) + i64::from(work_area.height - height) / 2)
            .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
        width,
        height,
    }
}

fn default_bounds() -> WindowBounds {
    WindowBounds {
        x: 0,
        y: 0,
        width: DEFAULT_WINDOW_WIDTH,
        height: DEFAULT_WINDOW_HEIGHT,
    }
}

fn clamp_to_default_limits(bounds: WindowBounds) -> WindowBounds {
    WindowBounds {
        x: bounds.x,
        y: bounds.y,
        width: bounds.width.clamp(MIN_WINDOW_WIDTH, MAX_WINDOW_DIMENSION),
        height: bounds.height.clamp(MIN_WINDOW_HEIGHT, MAX_WINDOW_DIMENSION),
    }
}

fn overlap_area(left: &MonitorGeometry, right: &MonitorGeometry) -> i128 {
    let left_right = i128::from(left.x) + i128::from(left.width);
    let right_right = i128::from(right.x) + i128::from(right.width);
    let left_bottom = i128::from(left.y) + i128::from(left.height);
    let right_bottom = i128::from(right.y) + i128::from(right.height);
    let width = (left_right.min(right_right) - i128::from(left.x).max(i128::from(right.x))).max(0);
    let height =
        (left_bottom.min(right_bottom) - i128::from(left.y).max(i128::from(right.y))).max(0);
    width * height
}

fn overlap_area_with_bounds(bounds: &WindowBounds, monitor: &MonitorGeometry) -> i128 {
    let window_right = i128::from(bounds.x) + i128::from(bounds.width);
    let monitor_right = i128::from(monitor.x) + i128::from(monitor.width);
    let window_bottom = i128::from(bounds.y) + i128::from(bounds.height);
    let monitor_bottom = i128::from(monitor.y) + i128::from(monitor.height);
    let width =
        (window_right.min(monitor_right) - i128::from(bounds.x).max(i128::from(monitor.x))).max(0);
    let height = (window_bottom.min(monitor_bottom)
        - i128::from(bounds.y).max(i128::from(monitor.y)))
    .max(0);
    width * height
}

fn center_distance_squared(bounds: &WindowBounds, monitor: &MonitorGeometry) -> i128 {
    let window_center_x = i128::from(bounds.x) * 2 + i128::from(bounds.width);
    let window_center_y = i128::from(bounds.y) * 2 + i128::from(bounds.height);
    let monitor_center_x = i128::from(monitor.x) * 2 + i128::from(monitor.width);
    let monitor_center_y = i128::from(monitor.y) * 2 + i128::from(monitor.height);
    let dx = window_center_x - monitor_center_x;
    let dy = window_center_y - monitor_center_y;
    dx * dx + dy * dy
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn monitor(
        name: &str,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        work_area: WorkArea,
    ) -> MonitorGeometry {
        MonitorGeometry {
            name: Some(name.to_owned()),
            x,
            y,
            width,
            height,
            work_area,
            scale_factor: 1.0,
        }
    }

    #[test]
    fn save_and_load_round_trip() {
        let directory = tempdir().expect("temporary directory");
        let store = WindowStateStore::from_app_data_dir(directory.path());
        let state = WindowState {
            normal: WindowBounds {
                x: -1200,
                y: 80,
                width: 1400,
                height: 800,
            },
            display_state: WindowDisplayState::Maximized,
            monitor: Some(SavedMonitor {
                name: Some("DISPLAY-1".to_owned()),
                x: -1920,
                y: 0,
                width: 1920,
                height: 1080,
                work_area: WorkArea {
                    x: -1920,
                    y: 0,
                    width: 1920,
                    height: 1040,
                },
                scale_factor: 1.25,
            }),
            ..WindowState::default()
        };

        assert_eq!(store.save(&state).expect("save"), SaveResult::Saved);
        assert_eq!(store.load().expect("load"), Some(state));
    }

    #[test]
    fn malformed_and_old_versions_are_rejected() {
        let directory = tempdir().expect("temporary directory");
        let store = WindowStateStore::from_app_data_dir(directory.path());
        fs::write(store.path(), b"not json").expect("write malformed state");
        assert_eq!(
            store
                .load()
                .expect_err("malformed state should fail")
                .kind(),
            ErrorKind::InvalidData
        );

        let old_state = serde_json::json!({
            "version": 0,
            "normal": {"x": 0, "y": 0, "width": 1600, "height": 960},
            "display_state": "normal"
        });
        fs::write(
            store.path(),
            serde_json::to_vec(&old_state).expect("serialize old state"),
        )
        .expect("write old state");
        assert_eq!(
            store.load().expect_err("old state should fail").kind(),
            ErrorKind::InvalidData
        );
    }

    #[test]
    fn minimized_state_is_not_a_restorable_display_state() {
        let value = serde_json::json!({
            "version": WINDOW_STATE_VERSION,
            "normal": {"x": 0, "y": 0, "width": 1600, "height": 960},
            "display_state": "minimized"
        });
        assert!(serde_json::from_value::<WindowState>(value).is_err());
    }

    #[test]
    fn restore_clamps_to_named_negative_coordinate_monitor() {
        let left = monitor(
            "LEFT",
            -1920,
            0,
            1920,
            1080,
            WorkArea {
                x: -1920,
                y: 0,
                width: 1920,
                height: 1040,
            },
        );
        let right = monitor(
            "RIGHT",
            0,
            0,
            2560,
            1440,
            WorkArea {
                x: 0,
                y: 0,
                width: 2560,
                height: 1400,
            },
        );
        let state = WindowState {
            normal: WindowBounds {
                x: 50_000,
                y: -50_000,
                width: 50_000,
                height: 50_000,
            },
            monitor: Some(SavedMonitor {
                name: Some("LEFT".to_owned()),
                x: -1920,
                y: 0,
                width: 1920,
                height: 1080,
                work_area: left.work_area,
                scale_factor: 1.0,
            }),
            ..WindowState::default()
        };
        let (bounds, display_state) = restore_placement(Some(&state), &[left, right], None);
        assert_eq!(display_state, WindowDisplayState::Normal);
        assert_eq!(bounds.x, -1920);
        assert_eq!(bounds.y, 0);
        assert_eq!(bounds.width, 1920);
        assert_eq!(bounds.height, 1040);
    }

    #[test]
    fn default_window_is_centered_on_primary_monitor() {
        let primary = monitor(
            "PRIMARY",
            0,
            0,
            2560,
            1440,
            WorkArea {
                x: 0,
                y: 40,
                width: 2560,
                height: 1400,
            },
        );
        let (bounds, display_state) = restore_placement(None, &[primary.clone()], Some(&primary));
        assert_eq!(display_state, WindowDisplayState::Normal);
        assert_eq!(bounds.width, DEFAULT_WINDOW_WIDTH);
        assert_eq!(bounds.height, DEFAULT_WINDOW_HEIGHT);
        assert_eq!(bounds.x, (2560 - DEFAULT_WINDOW_WIDTH) as i32 / 2);
        assert_eq!(bounds.y, 40 + (1400 - DEFAULT_WINDOW_HEIGHT) as i32 / 2);
    }

    #[test]
    fn small_work_area_still_produces_a_fitting_window() {
        let bounds = fit_bounds_to_work_area(
            WindowBounds {
                x: -5000,
                y: 5000,
                width: 10,
                height: 10,
            },
            WorkArea {
                x: -800,
                y: -600,
                width: 800,
                height: 600,
            },
        );
        assert_eq!(
            bounds,
            WindowBounds {
                x: -800,
                y: -600,
                width: 800,
                height: 600,
            }
        );
    }

    #[test]
    fn save_is_skipped_when_another_process_holds_the_lock() {
        let directory = tempdir().expect("temporary directory");
        let store = WindowStateStore::from_app_data_dir(directory.path());
        fs::write(
            store.path().with_file_name("window-state.json.lock"),
            b"pid=other",
        )
        .expect("create lock");
        assert_eq!(
            store.save(&WindowState::default()).expect("save result"),
            SaveResult::SkippedLocked
        );
        assert!(!store.path().exists());
    }

    #[test]
    fn reset_removes_state() {
        let directory = tempdir().expect("temporary directory");
        let store = WindowStateStore::from_app_data_dir(directory.path());
        store.save(&WindowState::default()).expect("save");
        assert_eq!(store.reset().expect("reset"), ResetResult::Reset);
        assert_eq!(store.load().expect("load after reset"), None);
    }
}
