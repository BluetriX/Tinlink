// Microphone level meter. Runs on the audio thread, so it keeps working in
// background tabs where timers are throttled; posts the RMS level in dBFS
// about every 20 ms.
"use strict";

class LevelMeter extends AudioWorkletProcessor {
  constructor() {
    super();
    this.sum = 0;
    this.count = 0;
  }

  process(inputs) {
    const channel = inputs[0]?.[0];
    if (channel) {
      for (let i = 0; i < channel.length; i++) this.sum += channel[i] * channel[i];
      this.count += channel.length;
    }
    // 8 render quanta of 128 frames: about 21 ms at 48 kHz.
    if (this.count >= 1024) {
      const rms = Math.sqrt(this.sum / this.count);
      this.port.postMessage(rms > 0 ? 20 * Math.log10(rms) : -100);
      this.sum = 0;
      this.count = 0;
    }
    return true;
  }
}

registerProcessor("level-meter", LevelMeter);
