"""VoiceMixer共有PCM v1。x64 SPSC、読書きはステレオフレーム単位。"""
import mmap
import struct


class SharedAudioRing:
    def __init__(self, path):
        self.file = open(path, "r+b", buffering=0)
        self.mapping = mmap.mmap(self.file.fileno(), 0)
        try:
            magic, version, rate, channels, _, capacity, _, _, _ = struct.unpack_from("<8sIIHH4xQQQ16s", self.mapping)
            if magic != b"ARPCM001" or version != 1 or rate != 48000 or channels != 2 or capacity < 2 or capacity % 2 or len(self.mapping) != 64 + capacity * 4:
                raise ValueError("共有PCM形式が不正です（48 kHzステレオv1が必要）")
            self.capacity = capacity
        except Exception:
            self.close()
            raise

    def counters(self):
        read, write = struct.unpack_from("<QQ", self.mapping, 32)
        if write < read or write - read > self.capacity:
            raise ValueError("共有PCMのカウンターが不正です")
        return read, write

    @property
    def available_read(self):
        read, write = self.counters()
        return write - read

    def pop(self, maximum):
        read, write = self.counters()
        count = min(maximum, write - read) // 2 * 2
        first = min(count, self.capacity - read % self.capacity)
        values = list(struct.unpack_from(f"<{first}f", self.mapping, 64 + read % self.capacity * 4))
        if count > first:
            values.extend(struct.unpack_from(f"<{count-first}f", self.mapping, 64))
        struct.pack_into("<Q", self.mapping, 32, read + count)
        return values

    def push(self, values):
        read, write = self.counters()
        count = min(len(values), self.capacity - (write - read)) // 2 * 2
        first = min(count, self.capacity - write % self.capacity)
        struct.pack_into(f"<{first}f", self.mapping, 64 + write % self.capacity * 4, *values[:first])
        if count > first:
            struct.pack_into(f"<{count-first}f", self.mapping, 64, *values[first:count])
        struct.pack_into("<Q", self.mapping, 40, write + count)
        return count

    def close(self):
        self.mapping.close()
        self.file.close()
