import { describe, expect, it } from "vitest";
import { hotkeySchema, settingsSchema } from "./settings";
describe("persistent hotkey contract", () => {
  it("accepts defaults and physical sides, any side and ContextMenu", () => {
    expect(
      settingsSchema.parse({
        hotkeys: {
          slow: null,
          fast: null,
          scrollHorizontal: null,
          scrollVertical: null,
          scrollBoth: null,
        },
        autostart: false,
      }).autostart,
    ).toBe(false);
    for (const keys of [
      ["ControlLeft", "KeyA"],
      ["Control", "KeyA"],
      ["MetaRight", "ContextMenu"],
    ])
      expect(hotkeySchema.safeParse({ keys }).success).toBe(true);
  });
  it("rejects empty, duplicate, unknown and ambiguous modifier keys", () => {
    for (const keys of [
      [],
      ["F13", "F13"],
      ["KeyЖ"],
      ["Control", "ControlRight"],
      ["WindowsScan9"],
    ])
      expect(hotkeySchema.safeParse({ keys }).success).toBe(false);
  });
});
