//! 対話収録の台帳と非破壊の無音短縮。個人音声は呼び出し元指定領域に保存する。
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

pub const PROMPTS: [(&str, &str, bool); 10] = [
    ("room", "室内音のみ：話さずに数秒待ってください。", true),
    (
        "normal",
        "普段の声で：今日は窓を開けて、ゆっくり深呼吸をしました。",
        false,
    ),
    (
        "quiet",
        "小さな声で：静かな部屋で、小さな時計の音が聞こえます。",
        false,
    ),
    (
        "loud",
        "無理のない大きな声で：おはようございます。こちらの声は届いていますか。",
        false,
    ),
    (
        "onset",
        "2秒待ち、はっきり発声：ぱっと明かりがつきました。最後に2秒待つ。",
        false,
    ),
    (
        "pause",
        "途中で2秒休む：ここで一度止まります。（休止）それでは続きを話します。",
        false,
    ),
    (
        "vowel",
        "息が苦しくならない長さで：あー、いー、うー、えー、おー。",
        false,
    ),
    (
        "dynamic",
        "小声から普段の声へ：遠くから近くへ、声の大きさを変えてみます。",
        false,
    ),
    (
        "ending",
        "語尾を柔らかく：今日のお話は、これでおしまいです。その後3秒待つ。",
        false,
    ),
    (
        "consonant",
        "自然な声で：ささやく風と、かすかな足音が聞こえました。",
        false,
    ),
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Quality {
    pub peak: f32,
    pub rms: f32,
    pub clipped_samples: usize,
    pub longest_clip_run: usize,
    pub has_voice_level: bool,
    pub low_level_warning: bool,
}
pub fn quality(samples: &[f32], low_db: f32, room: bool) -> Quality {
    let mut peak = 0.0f32;
    let mut clipped = 0;
    let mut run = 0;
    let mut longest = 0;
    let mut sum = 0.0f64;
    for &sample in samples {
        peak = peak.max(sample.abs());
        sum += f64::from(sample).powi(2);
        if sample.abs() >= 0.999 {
            clipped += 1;
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    let rms = (sum / samples.len().max(1) as f64).sqrt() as f32;
    let has_voice_level = peak >= 10.0f32.powf(low_db / 20.0);
    Quality {
        peak,
        rms,
        clipped_samples: clipped,
        longest_clip_run: longest,
        has_voice_level,
        low_level_warning: !room && !has_voice_level,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SilenceOptions {
    pub threshold_db: f32,
    pub minimum_ms: u32,
    pub edge_padding_ms: u32,
}
impl Default for SilenceOptions {
    fn default() -> Self {
        Self {
            threshold_db: -45.0,
            minimum_ms: 100,
            edge_padding_ms: 10,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SilenceEdit {
    pub original_frames: usize,
    pub output_frames: usize,
    pub removed_frame_ranges: Vec<[usize; 2]>,
    pub options: SilenceOptions,
}

pub fn shorten_silences(
    samples: &[f32],
    channels: usize,
    rate: u32,
    options: &SilenceOptions,
) -> Result<(Vec<f32>, SilenceEdit), String> {
    if channels == 0
        || rate == 0
        || samples.len() % channels != 0
        || samples.iter().any(|s| !s.is_finite())
        || !options.threshold_db.is_finite()
        || !(-120.0..=0.0).contains(&options.threshold_db)
        || options.edge_padding_ms > 50
    {
        return Err("音声形式または無音短縮設定が不正です（余白は0〜50 ms）".into());
    }
    let frames = samples.len() / channels;
    let keep = (rate as usize / 10).max(1);
    let minimum = ((u64::from(options.minimum_ms) * u64::from(rate) / 1000) as usize).max(keep + 1);
    let padding = (u64::from(options.edge_padding_ms) * u64::from(rate) / 1000) as usize;
    let threshold = 10.0f32.powf(options.threshold_db / 20.0);
    let mut ranges = Vec::new();
    let mut start = None;
    for index in 0..=frames {
        let silent = index < frames
            && samples[index * channels..(index + 1) * channels]
                .iter()
                .all(|s| s.abs() < threshold);
        if silent {
            start.get_or_insert(index);
        } else if let Some(begin) = start.take() {
            if index - begin >= minimum {
                // 両発声端の余白を確保し、残りを後半側に残す。総残存長は100 ms。
                let left = padding.max((rate as usize / 1000).max(1)).min(keep / 2);
                ranges.push([begin + left, index - (keep - left)]);
            }
        }
    }
    let mut out = Vec::with_capacity(samples.len());
    let mut cursor = 0;
    let mut fade_ins = Vec::new();
    let fade = (rate as usize / 1000).min(keep / 2);
    for &[a, b] in &ranges {
        out.extend_from_slice(&samples[cursor * channels..a * channels]);
        // 無音側だけを1 msフェードし、発声には触れない。
        let out_frames = out.len() / channels;
        for n in 0..fade.min(a - cursor) {
            let gain = n as f32 / fade as f32;
            for c in 0..channels {
                out[(out_frames - 1 - n) * channels + c] *= gain;
            }
        }
        cursor = b;
        fade_ins.push(out.len() / channels);
    }
    out.extend_from_slice(&samples[cursor * channels..]);
    for begin in fade_ins {
        for n in 0..fade.min(out.len() / channels - begin) {
            for c in 0..channels {
                out[(begin + n) * channels + c] *= n as f32 / fade as f32;
            }
        }
    }
    let edit = SilenceEdit {
        original_frames: frames,
        output_frames: out.len() / channels,
        removed_frame_ranges: ranges,
        options: options.clone(),
    };
    Ok((out, edit))
}

pub fn read_wav(path: &Path) -> Result<(hound::WavSpec, Vec<f32>), String> {
    let mut reader = hound::WavReader::open(path).map_err(|e| e.to_string())?;
    let spec = reader.spec();
    if spec.sample_format != hound::SampleFormat::Float || spec.bits_per_sample != 32 {
        return Err("収録したFloat32 WAVを指定してください".into());
    }
    let samples = reader
        .samples::<f32>()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    Ok((spec, samples))
}

pub fn derive_shortened(
    input: &Path,
    output: &Path,
    options: &SilenceOptions,
) -> Result<SilenceEdit, String> {
    let (spec, samples) = read_wav(input)?;
    let (out, edit) =
        shorten_silences(&samples, spec.channels as usize, spec.sample_rate, options)?;
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .map_err(|e| e.to_string())?;
    let mut writer = hound::WavWriter::new(file, spec).map_err(|e| e.to_string())?;
    for sample in out {
        writer.write_sample(sample).map_err(|e| e.to_string())?;
    }
    writer.finalize().map_err(|e| e.to_string())?;
    Ok(edit)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Take {
    pub id: String,
    pub recorded_at: String,
    pub file: String,
    pub endpoint_id: String,
    pub duration_ms: u64,
    pub status: String,
    pub quality: Option<Quality>,
    pub error: Option<String>,
    pub derived: Vec<Derived>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Derived {
    pub file: String,
    pub source_take_id: String,
    pub edit: SilenceEdit,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptRecord {
    pub id: String,
    pub text: String,
    pub room_tone: bool,
    pub accepted_take: Option<String>,
    pub takes: Vec<Take>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub schema_version: u32,
    pub endpoint_id: String,
    pub low_level_db: f32,
    pub silence: SilenceOptions,
    pub prompts: Vec<PromptRecord>,
}
impl Session {
    pub fn new(endpoint_id: String) -> Self {
        Self {
            schema_version: 1,
            endpoint_id,
            low_level_db: -40.0,
            silence: SilenceOptions::default(),
            prompts: PROMPTS
                .iter()
                .map(|(id, text, room)| PromptRecord {
                    id: (*id).into(),
                    text: (*text).into(),
                    room_tone: *room,
                    accepted_take: None,
                    takes: Vec::new(),
                })
                .collect(),
        }
    }
    pub fn load(directory: &Path) -> Result<Self, String> {
        let data = fs::read(directory.join("session.json")).map_err(|e| e.to_string())?;
        let session: Self = serde_json::from_slice(&data).map_err(|e| e.to_string())?;
        if session.schema_version != 1 {
            return Err("未対応の収録台帳バージョンです".into());
        }
        Ok(session)
    }
    pub fn save(&self, directory: &Path) -> Result<(), String> {
        use std::io::Write;
        fs::create_dir_all(directory).map_err(|e| e.to_string())?;
        let temporary = directory.join(format!("session-{}.tmp", uuid::Uuid::new_v4()));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|e| e.to_string())?;
        file.write_all(&serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        drop(file);
        fs::rename(&temporary, directory.join("session.json")).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shortens_leading_middle_trailing_but_preserves_short_and_one_sided_voice() {
        let mut samples = vec![0.0; 600]; // 300 ms stereo
        samples.extend(vec![0.2; 40]);
        samples.extend(vec![0.0; 400]);
        for _ in 0..200 {
            samples.extend([0.0, 0.2]);
        }
        samples.extend(vec![0.0; 100]); // 50 ms preserved
        samples.extend(vec![0.2; 40]);
        samples.extend(vec![0.0; 800]);
        let before = samples.clone();
        let (out, edit) = shorten_silences(&samples, 2, 1000, &SilenceOptions::default()).unwrap();
        assert_eq!(edit.removed_frame_ranges.len(), 3);
        assert_eq!(out.len(), samples.len() - (200 + 100 + 300) * 2);
        assert_eq!(samples, before);
        assert_eq!(
            out.chunks_exact(2)
                .filter(|f| f[1] == 0.2 && f[0] == 0.0)
                .count(),
            200
        );
    }
    #[test]
    fn quality_distinguishes_room_and_clipping() {
        assert!(!quality(&[0.0; 50], -40.0, true).low_level_warning);
        assert!(quality(&[0.0; 50], -40.0, false).low_level_warning);
        let q = quality(&[1.0, -1.0, 0.0, 1.0], -40.0, false);
        assert_eq!((q.clipped_samples, q.longest_clip_run), (3, 2));
    }
    #[test]
    fn rejects_invalid_audio_and_options() {
        assert!(shorten_silences(&[f32::NAN], 1, 1000, &SilenceOptions::default()).is_err());
        assert!(shorten_silences(&[0.0], 2, 1000, &SilenceOptions::default()).is_err());
    }
    #[test]
    fn session_replacement_and_original_preservation() {
        let dir = std::env::temp_dir().join(format!("vm-recorder-test-{}", uuid::Uuid::new_v4()));
        let mut session = Session::new("endpoint".into());
        session.save(&dir).unwrap();
        session.low_level_db = -35.0;
        session.save(&dir).unwrap();
        assert_eq!(Session::load(&dir).unwrap().low_level_db, -35.0);
        let original = dir.join("original.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 1000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut writer = hound::WavWriter::create(&original, spec).unwrap();
        for sample in vec![0.0001f32; 220]
            .into_iter()
            .chain(vec![0.2; 40])
            .chain(vec![0.0001; 400])
        {
            writer.write_sample(sample).unwrap();
        }
        writer.finalize().unwrap();
        let bytes = fs::read(&original).unwrap();
        let derived = dir.join("derived.wav");
        let derived_again = dir.join("derived-again.wav");
        let edit = derive_shortened(&original, &derived, &session.silence).unwrap();
        assert_eq!(edit.output_frames, 220); // 110 ms + 20 ms voice + 200 ms -> 100 + 20 + 100
        assert_eq!(edit.removed_frame_ranges.len(), 2);
        derive_shortened(&original, &derived_again, &session.silence).unwrap();
        assert_eq!(
            fs::read(&derived).unwrap(),
            fs::read(&derived_again).unwrap()
        );
        assert_eq!(fs::read(&original).unwrap(), bytes);
        assert!(derive_shortened(&original, &original, &session.silence).is_err());
        let (_, audio) = read_wav(&derived).unwrap();
        assert_eq!(audio.iter().filter(|&&s| s == 0.2).count(), 40);
        for path in [original, derived, derived_again] {
            fs::remove_file(path).unwrap();
        }
        fs::remove_file(dir.join("session.json")).unwrap();
        fs::remove_dir(dir).unwrap();
    }
}
