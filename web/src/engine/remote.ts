// The page's side of the machine's worker (worker.ts): requests that get a reply, orders that do not, and
// what the worker says by itself (a run done, the machine stopped).

import type { FromWorker, Request, ToWorker } from './protocol.ts';

type Without<T, K extends keyof T> = T extends unknown ? Omit<T, K> : never;
type Order = Exclude<ToWorker, Request>;

export interface RemoteEvents {
  /** A run of frames is done: how many ran. */
  ran(frames: number): void;
  /** The machine stopped on an error of its own, or the worker could not start. */
  failed(error: string): void;
}

export class Remote {
  private readonly worker: Worker;
  private readonly calls = new Map<number, { resolve(value: unknown): void; reject(error: Error): void }>();
  private nextId = 1;
  /** Why every call fails from now on: the worker did not start, or was stopped. */
  private broken: Error | null = null;

  constructor(events: RemoteEvents) {
    this.worker = new Worker(new URL('./worker.ts', import.meta.url), { type: 'module' });
    this.worker.onmessage = ({ data }: MessageEvent<FromWorker>) => {
      switch (data.kind) {
        case 'ran':
          return events.ran(data.frames);
        case 'failed':
          return events.failed(data.error);
        case 'reply': {
          const call = this.calls.get(data.id);
          this.calls.delete(data.id);
          if (data.ok) call?.resolve(data.value);
          else call?.reject(new Error(data.error));
        }
      }
    };
    this.worker.onerror = (event) => {
      event.preventDefault();
      this.break(new Error(event.message || 'the emulator could not start'));
      events.failed(this.broken!.message);
    };
  }

  /** An order the worker carries out in turn, with no reply. */
  post(message: Order, transfer: Transferable[] = []): void {
    if (!this.broken) this.worker.postMessage(message, transfer);
  }

  /** A request, and its reply; rejected if the worker fails or stops first. */
  call<T>(message: Without<Request, 'id'>): Promise<T> {
    if (this.broken) return Promise.reject(this.broken);
    const id = this.nextId++;
    return new Promise<T>((resolve, reject) => {
      this.calls.set(id, { resolve: resolve as (value: unknown) => void, reject });
      this.worker.postMessage({ ...message, id });
    });
  }

  /** Stops the worker; what was asked and not answered fails. */
  terminate(): void {
    this.worker.terminate();
    this.break(new Error('closed'));
  }

  private break(error: Error): void {
    this.broken ??= error;
    for (const call of this.calls.values()) call.reject(this.broken);
    this.calls.clear();
  }
}
