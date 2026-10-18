"""公式Python APIによる実推論。モデル自動取得・原音フォールバックは禁止。"""
from pathlib import Path
import math
import os

from model_manager import default_root, descriptor, installed

DEFAULTS = {"method": "deepfilternet3", "device": "cpu", "chunk_seconds": 1.0, "model_root": ""}


def parameters(update, previous=None):
    values = {**DEFAULTS, **(previous or {})}
    if not isinstance(update, dict) or set(update) - set(DEFAULTS):
        raise ValueError("未定義のパラメーターです")
    values.update(update)
    if values["method"] not in ("deepfilternet3", "htdemucs", "htdemucs_ft") or values["device"] not in ("cpu", "cuda"):
        raise ValueError("方式またはデバイスが不正です")
    seconds = values["chunk_seconds"]
    if isinstance(seconds, bool) or not isinstance(seconds, (int, float)) or not math.isfinite(seconds) or not 0.25 <= seconds <= 7.0:
        raise ValueError("処理チャンクは0.25〜7秒です")
    if not isinstance(values["model_root"], str):
        raise ValueError("モデル保存先は文字列です")
    return values


class ExtractionEngine:
    def __init__(self, settings):
        self.settings = parameters(settings)
        self.method = self.settings["method"]
        self.device = self.settings["device"]
        root = Path(self.settings["model_root"]) if self.settings["model_root"] else default_root()
        if not installed(root, self.method, verify=True):
            raise RuntimeError("モデル未導入です。モデル管理から明示導入してください")
        folder = (root / self.method).resolve()
        # DeepFilterNet configは環境変数DEVICEを優先する。プロセス単位の設定。
        os.environ["DEVICE"] = self.device
        import torch
        import torchaudio
        self.torch, self.torchaudio = torch, torchaudio
        if self.device == "cuda" and not torch.cuda.is_available():
            raise RuntimeError("CUDAを利用できません。CPUへ暗黙切替はしません")
        torch.set_num_threads(min(4, os.cpu_count() or 1))
        if self.method == "deepfilternet3":
            from df.enhance import init_df, enhance
            loaded = init_df(str(folder / "DeepFilterNet3"), log_level="ERROR", log_file=None)
            self.model, self.state = loaded[:2]
            self.enhance = enhance
            if self.state.sr() != 48000:
                raise ValueError("DeepFilterNetモデルのサンプルレートが不正です")
        else:
            from demucs.pretrained import get_model
            from demucs.apply import apply_model
            self.model = get_model(descriptor(self.method)["model_entry"], repo=folder)
            self.apply_model = apply_model
            self.vocal_index = self.model.sources.index("vocals")
        self.context_frames = 2400 if self.method == "deepfilternet3" else 12000
        self.chunk_frames = round(self.settings["chunk_seconds"] * 48000)
        self.latency_frames = self.chunk_frames + self.context_frames

    def process_window(self, samples):
        torch = self.torch
        audio = torch.tensor(samples, dtype=torch.float32).reshape(-1, 2).t().contiguous()
        if not torch.isfinite(audio).all():
            raise ValueError("入力音声が有限値ではありません")
        with torch.inference_mode():
            if self.method == "deepfilternet3":
                output = self.enhance(self.model, self.state, audio, pad=True)
            else:
                rate = self.model.samplerate
                normalized = self.torchaudio.functional.resample(audio, 48000, rate)
                reference = normalized.mean(0)
                mean, scale = reference.mean(), reference.std().clamp_min(1e-8)
                normalized = (normalized - mean) / scale
                stems = self.apply_model(self.model, normalized[None], shifts=0, split=True, overlap=0.25, progress=False, device=self.device, num_workers=0, segment=7.5)
                output = stems[0, self.vocal_index].cpu() * scale + mean
                output = self.torchaudio.functional.resample(output, rate, 48000)
            if output.shape[-1] < audio.shape[-1]:
                output = torch.nn.functional.pad(output, (0, audio.shape[-1] - output.shape[-1]))
            output = output[:, :audio.shape[-1]]
            if not torch.isfinite(output).all():
                raise ValueError("推論結果が有限値ではありません")
            return output.t().contiguous().reshape(-1).tolist()

    def close(self):
        self.model = None
        if self.device == "cuda":
            self.torch.cuda.empty_cache()
