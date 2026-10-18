//! 描画とヒット領域で同じ配置を使い、各候補の直下へメーターを描く。
use super::*;

fn input_status(state: &DisplayState, command: Command) -> Option<&'static str> {
    let mode = match command {
        Command::Device => Mode::Device,
        Command::Application => Mode::Application,
        Command::Browser => Mode::Browser,
        _ => return None,
    };
    if state.mode != mode {
        return Some("無効");
    }
    match state.phase {
        Phase::Starting => Some("準備中"),
        Phase::Waiting => Some("待機中"),
        Phase::Error => Some("エラー"),
        Phase::Stopped | Phase::Exited => Some("無効"),
        Phase::Running => None,
    }
}

unsafe fn draw_meter(
    dc: HDC,
    rect: RECT,
    bins: &[Option<f32>; BINS],
    color: u32,
    status: Option<&str>,
) {
    unsafe {
        if let Some(status) = status {
            draw_text(dc, status, rect, DT_CENTER | DT_VCENTER, 0x00aaa29c);
            return;
        }
        for (index, value) in bins.iter().enumerate() {
            let width = rect.right - rect.left;
            let left = rect.left + index as i32 * width / BINS as i32;
            let bar = RECT {
                left,
                right: rect.left + (index + 1) as i32 * width / BINS as i32 - 1,
                ..rect
            };
            fill(dc, &bar, 0x00433830);
            if let Some(value) = value {
                let height =
                    (level_height(*value) * (rect.bottom - rect.top) as f32).round() as i32;
                if height > 0 {
                    fill(
                        dc,
                        &RECT {
                            top: rect.bottom - height,
                            ..bar
                        },
                        if *value >= 1.0 { 0x004040ff } else { color },
                    );
                }
            }
        }
    }
}

pub(super) unsafe fn draw_overlay(dc: HDC, bounds: RECT, state: &DisplayState) {
    unsafe {
        fill(dc, &bounds, 0x002d241f);
        fill(dc, &RECT { right: 6, ..bounds }, 0x005ac0);
        SetBkMode(dc, TRANSPARENT);
        let small = font(12);
        let previous = SelectObject(dc, small.into());
        for (label, top) in [("設定", 8), ("入力", 35), ("出力", 82)] {
            draw_text(
                dc,
                label,
                RECT {
                    left: 18,
                    top,
                    right: 55,
                    bottom: top + 23,
                },
                DT_LEFT | DT_VCENTER,
                0x00ffffff,
            );
        }
        let meters = *LEVELS.lock().unwrap();
        for command in CONTROLS {
            let Some(rect) = control_rect(command, state.inputs) else {
                continue;
            };
            let (mut label, active) = state.control_label(command);
            let disabled = state.editing && command != Command::Show;
            fill(
                dc,
                &rect,
                if disabled {
                    0x00453c36
                } else if active {
                    0x003c73bd
                } else {
                    0x004e4036
                },
            );
            if matches!(command, Command::Output1 | Command::Output2) {
                let index = command as usize - Command::Output1 as usize;
                label = fit_output(
                    dc,
                    &state.labels[index],
                    if state.outputs[index] { "ON" } else { "OFF" },
                    rect.right - rect.left - 8,
                );
            }
            draw_text(
                dc,
                &label,
                rect,
                DT_CENTER | DT_VCENTER,
                if disabled { 0x008b8683 } else { 0x00ffffff },
            );
            if matches!(
                command,
                Command::Device | Command::Application | Command::Browser
            ) {
                let status = input_status(state, command)
                    .or_else(|| (!meters.fresh || !meters.active[0]).then_some("測定待ち"));
                draw_meter(
                    dc,
                    RECT {
                        top: INPUT_METER_RECT.top,
                        bottom: INPUT_METER_RECT.bottom - 2,
                        ..rect
                    },
                    &meters.bins[0],
                    0x0078d25d,
                    status,
                );
            } else if matches!(command, Command::Output1 | Command::Output2) {
                let index = command as usize - Command::Output1 as usize;
                let status = if !state.outputs[index] || state.mode == Mode::Stopped {
                    Some("無効")
                } else if state.phase == Phase::Error {
                    Some("エラー")
                } else if matches!(state.phase, Phase::Starting | Phase::Waiting) || !meters.fresh {
                    Some("待機中")
                } else if !meters.active[index + 1] {
                    Some("無効")
                } else {
                    None
                };
                draw_meter(
                    dc,
                    RECT {
                        top: OUTPUT_METER_RECT.top,
                        bottom: OUTPUT_METER_RECT.bottom - 2,
                        ..rect
                    },
                    &meters.bins[index + 1],
                    0x00edb35c,
                    status,
                );
            }
        }
        if state.failed {
            draw_text(
                dc,
                &state.message,
                RECT {
                    left: 18,
                    top: 132,
                    right: OVERLAY_WIDTH - 12,
                    bottom: 154,
                },
                DT_LEFT | DT_VCENTER,
                0x00aab4ff,
            );
        }
        SelectObject(dc, previous);
        let _ = DeleteObject(small.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_selected_input_shows_progress_or_audio_state() {
        let mut settings = Settings::default();
        settings.mode = Mode::Browser;
        for (phase, expected) in [
            (Phase::Starting, Some("準備中")),
            (Phase::Waiting, Some("待機中")),
            (Phase::Error, Some("エラー")),
            (Phase::Running, None),
        ] {
            let state = DisplayState::new(&settings, phase, "", "", false);
            assert_eq!(input_status(&state, Command::Browser), expected);
            assert_eq!(input_status(&state, Command::Device), Some("無効"));
        }
    }
}
