use crate::audio::{
    format::AudioBlock,
    graph::{ChannelInputMode, ChannelMapPreset, ChannelMapperConfig, ChannelSource, PortId},
};

pub fn process_in_place(block: &mut AudioBlock, config: &ChannelMapperConfig) {
    let channels = block.format.channels as usize;
    if channels < 2 {
        return;
    }
    let gains = output_gains(config);
    for frame in block.samples.chunks_exact_mut(channels) {
        let input = [frame[0], frame[1]];
        let output = match config.preset {
            ChannelMapPreset::StereoToMono => {
                let mono = (input[0] + input[1]) * 0.5;
                [mono, mono]
            }
            ChannelMapPreset::MonoToStereo | ChannelMapPreset::LeftToMono => [input[0], input[0]],
            ChannelMapPreset::RightToMono => [input[1], input[1]],
            ChannelMapPreset::SwapLeftRight => [input[1], input[0]],
            ChannelMapPreset::Custom => {
                let mut mapped = [0.0, 0.0];
                for entry in &config.custom_map {
                    if entry.source_channel < 2 && entry.destination_channel < 2 {
                        mapped[entry.destination_channel] +=
                            input[entry.source_channel] * entry.gain;
                    }
                }
                mapped
            }
        };
        frame[0] = output[0] * gains[0];
        frame[1] = output[1] * gains[1];
    }
}

fn output_gains(config: &ChannelMapperConfig) -> [f32; 2] {
    [
        if config.left_mute {
            0.0
        } else {
            10.0_f32.powf(config.left_gain_db / 20.0)
        },
        if config.right_mute {
            0.0
        } else {
            10.0_f32.powf(config.right_gain_db / 20.0)
        },
    ]
}

/// 二入力を合算せずPort別に変換する。未接続の第一入力を第二入力で代用しない。
pub fn process_ports(
    inputs: &[(PortId, &AudioBlock)],
    ordered: &[PortId],
    output: &mut AudioBlock,
    config: &ChannelMapperConfig,
) {
    output.clear();
    if config.input_mode == ChannelInputMode::Single {
        if let Some(block) = ordered.first().and_then(|id| {
            inputs
                .iter()
                .find(|(port, _)| port == id)
                .map(|(_, block)| *block)
        }) {
            let count = output.samples.len().min(block.samples.len());
            output.samples[..count].copy_from_slice(&block.samples[..count]);
            output.copy_latency_from(block);
            process_in_place(output, config);
        }
        return;
    }
    // パス遅延の代表は第一接続入力。音声自体は各Portを区別する。
    if let Some(block) = ordered.iter().find_map(|id| {
        inputs
            .iter()
            .find(|(port, _)| port == id)
            .map(|(_, block)| *block)
    }) {
        output.copy_latency_from(block);
    }
    let gains = output_gains(config);
    if config.port_map.is_empty() {
        for (channel, port) in ordered.iter().take(2).enumerate() {
            if let Some((_, block)) = inputs.iter().find(|(id, _)| id == port) {
                for (destination, source) in output
                    .samples
                    .chunks_exact_mut(2)
                    .zip(block.samples.chunks_exact(2))
                {
                    destination[channel] = (source[0] + source[1]) * 0.5 * gains[channel];
                }
            }
        }
    } else {
        for entry in &config.port_map {
            if entry.mute || entry.destination_channel >= 2 {
                continue;
            }
            if let Some((_, block)) = inputs.iter().find(|(id, _)| *id == entry.source_port_id) {
                let gain = 10.0_f32.powf(entry.gain_db / 20.0) * gains[entry.destination_channel];
                for (destination, source) in output
                    .samples
                    .chunks_exact_mut(2)
                    .zip(block.samples.chunks_exact(2))
                {
                    let value = match entry.source {
                        ChannelSource::Left => source[0],
                        ChannelSource::Right => source[1],
                        ChannelSource::Average => (source[0] + source[1]) * 0.5,
                    };
                    destination[entry.destination_channel] += value * gain;
                }
            }
        }
    }
}
