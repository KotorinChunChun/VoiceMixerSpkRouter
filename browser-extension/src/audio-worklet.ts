/// <reference path="./audio-worklet.d.ts" />

class VoiceMixerPcmProcessor extends AudioWorkletProcessor {
  process(inputs: Float32Array[][], outputs: Float32Array[][], _parameters: Record<string, Float32Array>): boolean {
    const channels = inputs[0];
    const left = channels?.[0];
    if (!left || left.length === 0) return true;
    const right = channels[1] || left;
    const interleaved = new Float32Array(left.length * 2);
    for (let index = 0; index < left.length; index += 1) {
      interleaved[index * 2] = left[index];
      interleaved[index * 2 + 1] = right[index] ?? left[index];
    }
    const output = outputs[0];
    for (let channel = 0; channel < output.length; channel += 1) {
      const destination = output[channel];
      const input = channels[channel] || left;
      destination.set(input.subarray(0, destination.length));
    }
    this.port.postMessage(interleaved.buffer, [interleaved.buffer]);
    return true;
  }
}

registerProcessor("voicemixer-pcm", VoiceMixerPcmProcessor);
