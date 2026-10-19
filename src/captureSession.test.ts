import { describe, expect, it } from "vitest";
import { CaptureSession } from "./captureSession";
const status = (capturing: boolean, available = true) => ({
  available,
  capturing,
  message: "Linux: мышей — 2, клавиатур — 3",
});

describe("capture acknowledgement lifecycle", () => {
  it("preserves the target through unrelated idle status before acknowledgement", () => {
    const capture = new CaptureSession();
    const id = capture.start("slow");
    expect(capture.observe(status(false))).toBe("waiting");
    expect(capture.acknowledge(id)).toBe(true);
    expect(capture.complete()).toBe("slow");
  });

  it("ends an acknowledged capture on timeout and a pending capture on engine loss", () => {
    const capture = new CaptureSession();
    capture.start("fast");
    expect(capture.observe(status(true))).toBe("active");
    expect(capture.observe(status(false))).toBe("ended");
    expect(capture.complete()).toBeNull();
    capture.start("slow");
    expect(capture.observe(status(false, false))).toBe("ended");
    expect(capture.complete()).toBeNull();
  });

  it("ignores a late acknowledgement after completion, cancellation or a new request", () => {
    const capture = new CaptureSession();
    const old = capture.start("slow");
    expect(capture.complete()).toBe("slow");
    const current = capture.start("scrollBoth");
    expect(capture.acknowledge(old)).toBe(false);
    expect(capture.isCurrent(old)).toBe(false);
    expect(capture.acknowledge(current)).toBe(true);
    capture.cancel();
    expect(capture.acknowledge(current)).toBe(false);
    expect(capture.complete()).toBeNull();
  });
});
