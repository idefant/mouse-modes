import { z } from "zod";
const keyCode =
  /^(Control|Shift|Alt|Meta)(Left|Right)?$|^Key[A-Z]$|^Digit[0-9]$|^F([1-9]|1[0-9]|2[0-4])$|^(Space|Enter|Escape|Tab|Backspace|Delete|Insert|Home|End|PageUp|PageDown|ArrowUp|ArrowDown|ArrowLeft|ArrowRight|CapsLock|NumLock|ScrollLock|PrintScreen|Pause|ContextMenu|Backquote|Minus|Equal|BracketLeft|BracketRight|Backslash|Semicolon|Quote|Comma|Period|Slash|Numpad[0-9]|NumpadAdd|NumpadSubtract|NumpadMultiply|NumpadDivide|NumpadDecimal|NumpadEnter)$/;
export const hotkeySchema = z.object({
  keys: z
    .array(z.string().regex(keyCode))
    .min(1)
    .max(16)
    .refine(
      (keys) => new Set(keys).size === keys.length,
      "Повторяющаяся клавиша",
    )
    .refine(
      (keys) =>
        ["Control", "Shift", "Alt", "Meta"].every(
          (modifier) =>
            !keys.includes(modifier) ||
            (!keys.includes(`${modifier}Left`) &&
              !keys.includes(`${modifier}Right`)),
        ),
      "Нельзя одновременно указать любую сторону и конкретную сторону",
    ),
});
export const modeKeys = [
  "slow",
  "fast",
  "scrollHorizontal",
  "scrollVertical",
  "scrollBoth",
] as const;
export type Mode = (typeof modeKeys)[number];
export const settingsSchema = z.object({
  hotkeys: z.object({
    slow: hotkeySchema.nullable(),
    fast: hotkeySchema.nullable(),
    scrollHorizontal: hotkeySchema.nullable(),
    scrollVertical: hotkeySchema.nullable(),
    scrollBoth: hotkeySchema.nullable(),
  }),
  autostart: z.boolean(),
});
export type Settings = z.infer<typeof settingsSchema>;
export type Hotkey = z.infer<typeof hotkeySchema>;
export const statusSchema = z.object({
  available: z.boolean(),
  message: z.string(),
  capturing: z.boolean(),
});
export type EngineStatus = z.infer<typeof statusSchema>;
export function formatKey(code: string): string {
  const names: Record<string, string> = {
    Control: "Ctrl",
    ControlLeft: "Л. Ctrl",
    ControlRight: "П. Ctrl",
    Shift: "Shift",
    ShiftLeft: "Л. Shift",
    ShiftRight: "П. Shift",
    Alt: "Alt",
    AltLeft: "Л. Alt",
    AltRight: "П. Alt",
    Meta: "Win",
    MetaLeft: "Л. Win",
    MetaRight: "П. Win",
    Space: "Пробел",
    ArrowUp: "↑",
    ArrowDown: "↓",
    ArrowLeft: "←",
    ArrowRight: "→",
    Backquote: "`",
    Minus: "−",
    Equal: "=",
    BracketLeft: "[",
    BracketRight: "]",
    Backslash: "\\",
    Semicolon: ";",
    Quote: "'",
    Comma: ",",
    Period: ".",
    Slash: "/",
  };
  return names[code] ?? code.replace(/^Key|^Digit/, "");
}
