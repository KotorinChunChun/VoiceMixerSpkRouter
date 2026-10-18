"""人声抽出Process Plugin v1。制御と有限長PCM処理を分離する。"""
import json
import math
import sys
import threading
import time
import traceback

from engines import ExtractionEngine, parameters
from shared_audio import SharedAudioRing

PLUGIN_ID = "voicemixer.voice-extraction"


class Worker:
    def __init__(self, output, factory=ExtractionEngine):
        self.output, self.factory = output, factory
        self.settings = parameters({})
        self.write_lock = threading.Lock()
        self.lifecycle = threading.RLock()
        self.running = threading.Event()
        self.closed = threading.Event()
        self.engine = self.input = self.out = self.thread = None
        self.inference_ms = 0.0
        self.processed_frames = 0
        self.slow_chunks = 0
        self.pending = []
        self.buffer = []
        self.last_stats = 0.0
        self.generation = 0

    def send(self, kind, payload):
        with self.write_lock:
            self.output.write(json.dumps({"type": kind, "payload": payload}, ensure_ascii=True, allow_nan=False) + "\n")
            self.output.flush()

    def reset_audio(self):
        self.generation += 1
        self.pending = []
        self.buffer = [0.0] * (self.engine.context_frames * 2)
        self.input.pop(self.input.available_read)

    def handle(self, message):
        kind, payload = message.get("type"), message.get("payload") or {}
        if kind == "set_parameters":
            updated = parameters(payload, self.settings)
            if self.engine and updated != self.settings:
                raise ValueError("方式・チャンク・デバイス・モデル変更にはノード再起動が必要です")
            self.settings = updated
        elif kind == "prepare":
            if self.engine:
                raise ValueError("prepareは一回だけです")
            transport = payload["transport"]
            if (payload["sample_rate"], payload["channels"]) != (48000, 2) or (transport["sample_rate"], transport["channels"]) != (48000, 2):
                raise ValueError("48 kHzステレオのPrepareが必要です")
            self.input = SharedAudioRing(transport["input_mapping_name"])
            self.out = SharedAudioRing(transport["output_mapping_name"])
            self.engine = self.factory(self.settings)
            self.reset_audio()
            self.send("hello", {"protocol_version": 1, "plugin_id": PLUGIN_ID, "plugin_version": "0.1.0"})
            self.send("ready", {"latency_frames": self.engine.latency_frames, "preferred_block_frames": payload["graph_block_frames"]})
            self.thread = threading.Thread(target=self.audio_loop, daemon=True)
            self.thread.start()
        elif kind == "start":
            if not self.engine:
                raise ValueError("prepareが必要です")
            self.running.set()
            self.send("state", {"state": "running", "message": None})
        elif kind == "stop":
            self.running.clear()
            with self.lifecycle:
                if self.engine:
                    self.reset_audio()
            self.send("state", {"state": "stopped", "message": None})
        elif kind == "ping":
            self.send("pong", {"sequence": payload["sequence"]})
        elif kind == "shutdown":
            return False
        # 未知の通知（ports等）はv1仕様に従い無視する。
        return True

    def audio_loop(self):
        try:
            needed = (self.engine.chunk_frames + self.engine.context_frames * 2) * 2
            while not self.closed.wait(0.001):
                if not self.running.is_set():
                    continue
                with self.lifecycle:
                    if self.pending:
                        count = self.out.push(self.pending)
                        del self.pending[:count]
                        continue
                    remaining = needed - len(self.buffer)
                    self.buffer.extend(self.input.pop(min(8192, remaining)))
                    if len(self.buffer) < needed:
                        continue
                    generation = self.generation
                    window = self.buffer[:]
                start = time.perf_counter()
                output = self.engine.process_window(window)
                elapsed = time.perf_counter() - start
                if len(output) != len(window) or any(not math.isfinite(v) for v in output):
                    raise ValueError("推論結果の長さまたは値が不正です")
                with self.lifecycle:
                    if not self.running.is_set() or generation != self.generation:
                        continue
                    begin = self.engine.context_frames * 2
                    self.pending = output[begin:begin + self.engine.chunk_frames * 2]
                    del self.buffer[:self.engine.chunk_frames * 2]
                    self.inference_ms = elapsed * 1000
                    self.processed_frames += self.engine.chunk_frames
                    if elapsed > self.engine.chunk_frames / 48000:
                        self.slow_chunks += 1
                    self.send("stats", {"inference_ms": self.inference_ms, "queue_frames": self.input.available_read // 2 + len(self.buffer) // 2, "overruns": self.slow_chunks})
                    self.send("state", {"state": "running", "message": "推論が実時間に追いついていません（有限キュー）" if self.slow_chunks and elapsed > self.engine.chunk_frames / 48000 else None})
        except Exception as error:
            self.running.clear()
            self.send("error", {"code": "inference_failed", "message": str(error)})

    def close(self):
        self.closed.set()
        self.running.clear()
        if self.thread:
            self.thread.join(timeout=5)
            if self.thread.is_alive():
                # 推論中オブジェクトを解放しない。プロセス終了でOSに回収させる。
                return
        if self.engine:
            self.engine.close()
        if self.input:
            self.input.close()
        if self.out:
            self.out.close()


def main():
    sys.stdin.reconfigure(encoding="utf-8")
    sys.stdout.reconfigure(encoding="utf-8")
    protocol_stdout = sys.stdout
    sys.stdout = sys.stderr
    worker = Worker(protocol_stdout)
    try:
        for line in sys.stdin:
            try:
                if not worker.handle(json.loads(line)):
                    return 0
            except Exception as error:
                traceback.print_exc(file=sys.stderr)
                worker.send("error", {"code": "plugin_failure", "message": str(error)})
                # 不正な変更は拒否し、既存設定とPing応答を保持する。
        return 0
    finally:
        worker.close()


if __name__ == "__main__":
    raise SystemExit(main())
