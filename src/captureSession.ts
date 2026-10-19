import type { EngineStatus, Mode } from "./settings";

/** Tracks backend acknowledgement separately from unrelated device status events. */
export class CaptureSession {
  private generation = 0;
  private current: { id: number; mode: Mode; active: boolean } | null = null;

  start(mode: Mode): number {
    const id = ++this.generation;
    this.current = { id, mode, active: false };
    return id;
  }

  acknowledge(id: number): boolean {
    if (this.current?.id !== id) return false;
    this.current.active = true;
    return true;
  }

  isCurrent(id: number): boolean {
    return this.current?.id === id;
  }

  observe(status: EngineStatus): "waiting" | "active" | "ended" {
    if (!this.current) return "waiting";
    if (!status.available || (!status.capturing && this.current.active)) {
      this.cancel();
      return "ended";
    }
    if (status.capturing) {
      this.current.active = true;
      return "active";
    }
    return "waiting";
  }

  complete(): Mode | null {
    const mode = this.current?.mode ?? null;
    this.cancel();
    return mode;
  }

  cancel(): void {
    this.current = null;
  }
}
