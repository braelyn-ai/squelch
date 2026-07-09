// Pure-logic tests for the keymap dispatch core. Run with: `bun test`.
// No DOM / jsdom needed — dispatchCore is pure. These reproduce the systemic
// "no keys work" bug (modal context stuck on the stack) and prove the fix, plus
// the input guard, case-insensitive matching, latest-wins, and the resolved
// double-`t` collision.

import { expect, test, describe } from "bun:test";
import {
  dispatchCore,
  activeContext,
  isEditableTag,
  type KeyBinding,
  type RegisteredSet,
} from "./dispatchCore";

let seq = 0;
function set(context: RegisteredSet["context"], bindings: KeyBinding[]): RegisteredSet {
  return { id: `s${seq++}`, context, bindings };
}
function binding(key: string, spy: () => void, extra?: Partial<KeyBinding>): KeyBinding {
  return {
    key,
    description: key,
    handler: () => {
      spy();
    },
    ...extra,
  };
}

describe("activeContext", () => {
  test("defaults to list on the base stack", () => {
    expect(activeContext(["list"])).toBe("list");
  });
  test("topmost wins", () => {
    expect(activeContext(["list", "modal"])).toBe("modal");
  });
});

describe("the 'no keys work' bug (modal context stuck on the stack)", () => {
  // This is the exact production scenario before the fix: SideViews pushed
  // "modal" unconditionally at mount and never popped it, so the stack was
  // ["list","modal"] permanently while every real binding lived in "list".
  const jFired: string[] = [];
  const listSet = set("list", [
    binding("j", () => jFired.push("j")),
    binding("Enter", () => jFired.push("Enter")),
  ]);
  const modalSet = set("modal", [binding("Escape", () => jFired.push("Escape"))]);
  const sets = [listSet, modalSet];

  test("BUG REPRO: with modal stuck on the stack, list keys are dead", () => {
    jFired.length = 0;
    const r = dispatchCore({
      sets,
      contextStack: ["list", "modal"], // <- the stuck stack
      event: { key: "j" },
      editing: false,
    });
    expect(r.handled).toBe(false); // j never fires — this was the whole outage
    expect(jFired).toEqual([]);
  });

  test("FIX: with modal popped (side view closed), list keys fire again", () => {
    jFired.length = 0;
    const r = dispatchCore({
      sets,
      contextStack: ["list"], // <- fixed: modal only pushed while a panel is open
      event: { key: "j" },
      editing: false,
    });
    expect(r.handled).toBe(true);
    expect(r.firedContext).toBe("list");
    expect(jFired).toEqual(["j"]);
  });

  test("Escape still works while a modal is genuinely open", () => {
    jFired.length = 0;
    const r = dispatchCore({
      sets,
      contextStack: ["list", "modal"],
      event: { key: "Escape" },
      editing: false,
    });
    expect(r.handled).toBe(true);
    expect(r.firedContext).toBe("modal");
    expect(jFired).toEqual(["Escape"]);
  });
});

describe("input-focus guard", () => {
  test("single-letter list bindings are suppressed while editing", () => {
    const fired: string[] = [];
    const sets = [set("list", [binding("j", () => fired.push("j"))])];
    const r = dispatchCore({ sets, contextStack: ["list"], event: { key: "j" }, editing: true });
    expect(r.handled).toBe(false);
    expect(fired).toEqual([]);
  });
  test("allowInInput bindings still fire while editing", () => {
    const fired: string[] = [];
    const sets = [
      set("list", [binding("Escape", () => fired.push("esc"), { allowInInput: true })]),
    ];
    const r = dispatchCore({ sets, contextStack: ["list"], event: { key: "Escape" }, editing: true });
    expect(r.handled).toBe(true);
    expect(fired).toEqual(["esc"]);
  });
});

describe("key matching", () => {
  test("shift+letter matches a lowercase-registered binding (T -> t) case-insensitively", () => {
    const fired: string[] = [];
    const sets = [set("list", [binding("t", () => fired.push("t"))])];
    // Shift+T yields event.key === "T"; keyMatches falls back to lowercase.
    const r = dispatchCore({
      sets,
      contextStack: ["list"],
      event: { key: "T", shiftKey: true },
      editing: false,
    });
    expect(r.handled).toBe(true);
    expect(fired).toEqual(["t"]);
  });
  test("named keys encode shift (shift+Tab != Tab)", () => {
    const fired: string[] = [];
    const sets = [set("list", [binding("Tab", () => fired.push("tab"))])];
    const r = dispatchCore({
      sets,
      contextStack: ["list"],
      event: { key: "Tab", shiftKey: true },
      editing: false,
    });
    expect(r.handled).toBe(false); // normalizes to "shift+Tab", no match
    expect(fired).toEqual([]);
  });
});

describe("registration priority", () => {
  test("latest registration in a context wins (mounted-on-top)", () => {
    const fired: string[] = [];
    const sets = [
      set("list", [binding("x", () => fired.push("first"))]),
      set("list", [binding("x", () => fired.push("second"))]),
    ];
    dispatchCore({ sets, contextStack: ["list"], event: { key: "x" }, editing: false });
    expect(fired).toEqual(["second"]); // only the later one fires
  });

  test("resolved double-`t`: a single owner fires exactly once", () => {
    // After the fix, `t` is registered only by ActionLayer. Simulate the old
    // double registration vs the new single one to lock the behavior in.
    const fired: string[] = [];
    const singleOwner = [set("list", [binding("t", () => fired.push("tune"))])];
    dispatchCore({ sets: singleOwner, contextStack: ["list"], event: { key: "t" }, editing: false });
    expect(fired).toEqual(["tune"]);
  });
});

describe("isEditableTag", () => {
  test("INPUT/TEXTAREA/SELECT/contentEditable are editable; BODY is not", () => {
    expect(isEditableTag("INPUT", false)).toBe(true);
    expect(isEditableTag("TEXTAREA", false)).toBe(true);
    expect(isEditableTag("SELECT", false)).toBe(true);
    expect(isEditableTag("DIV", true)).toBe(true);
    expect(isEditableTag("BODY", false)).toBe(false);
    expect(isEditableTag("DIV", false)).toBe(false);
  });
});
