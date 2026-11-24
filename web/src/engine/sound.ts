// The page's side of the sound: an AudioContext with the worklet (sound-worklet.ts), and a channel from
// the machine's worker straight to it, so that the game's sound never waits on the page's thread.

import workletUrl from './sound-worklet.ts?worker&url';

export class Speaker {
  private readonly context: AudioContext;
  private readonly node: AudioWorkletNode;
  private readonly gain: GainNode;
  /** What the worklet last said: the sound waiting to play (ms), and the times it ran out. */
  report = { level: 0, dry: 0 };

  private constructor(context: AudioContext, node: AudioWorkletNode, gain: GainNode) {
    this.context = context;
    this.node = node;
    this.gain = gain;
    node.port.onmessage = ({ data }: MessageEvent<{ level: number; dry: number }>) => (this.report = data);
  }

  static async start(): Promise<Speaker> {
    const context = new AudioContext({ latencyHint: 'interactive' });
    try {
      await context.audioWorklet.addModule(workletUrl);
      const node = new AudioWorkletNode(context, 'sol-sound', { numberOfInputs: 0, outputChannelCount: [2] });
      const gain = context.createGain();
      node.connect(gain).connect(context.destination);
      return new Speaker(context, node, gain);
    } catch (error) { await context.close(); throw error; }
  }

  /** A port for the worker to send the game's sound on. */
  port(): MessagePort {
    const channel = new MessageChannel();
    this.node.port.postMessage({ port: channel.port1 }, [channel.port1]);
    return channel.port2;
  }

  /** Whether the browser lets it play yet: a page may only start sound after the player touched it. */
  get running(): boolean {
    return this.context.state === 'running';
  }

  /** Starts the sound, if a touch of the page now allows it. */
  resume(): void {
    if (this.context.state === 'suspended') void this.context.resume();
  }

  /** Lets go of what is waiting to play, as after a jump to a save state. */
  flush(): void {
    this.node.port.postMessage({ flush: true });
  }

  get muted(): boolean {
    return this.gain.gain.value === 0;
  }

  set muted(muted: boolean) {
    this.gain.gain.value = muted ? 0 : 1;
  }

  close(): void {
    void this.context.close();
  }
}
