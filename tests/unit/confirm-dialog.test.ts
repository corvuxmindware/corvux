import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, relative, sep } from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";
import { confirmDialog } from "../../src/lib/confirm";

// tauri-plugin-dialog replaces window.confirm with an async function on desktop
// and iOS, so a bare `if (!window.confirm(msg))` saw a (truthy) Promise and went
// ahead even when the user pressed Cancel. confirmDialog awaits the answer.
describe("confirmDialog", () => {
  const original = window.confirm;
  afterEach(() => {
    window.confirm = original;
  });

  const install = (impl: (msg: string) => unknown) => {
    window.confirm = vi.fn(impl) as unknown as typeof window.confirm;
  };

  it("returns false when the async (Tauri plugin) confirm resolves to Cancel", async () => {
    install(async () => false);
    await expect(confirmDialog("Delete?")).resolves.toBe(false);
  });

  it("returns true when the async (Tauri plugin) confirm resolves to OK", async () => {
    install(async () => true);
    await expect(confirmDialog("Delete?")).resolves.toBe(true);
  });

  it("does not resolve before the async confirm is answered", async () => {
    let answer!: (ok: boolean) => void;
    install(() => new Promise<boolean>((resolve) => (answer = resolve)));
    let settled = false;
    const pending = confirmDialog("Delete?").then((ok) => {
      settled = true;
      return ok;
    });
    await Promise.resolve();
    expect(settled).toBe(false);
    answer(false);
    await expect(pending).resolves.toBe(false);
  });

  it("handles the synchronous browser/Android confirm", async () => {
    install(() => false);
    await expect(confirmDialog("Delete?")).resolves.toBe(false);
    install(() => true);
    await expect(confirmDialog("Delete?")).resolves.toBe(true);
  });

  it("passes the message through unchanged", async () => {
    install(() => true);
    await confirmDialog('Delete "a.md"?\n\nRecoverable.');
    expect(window.confirm).toHaveBeenCalledWith('Delete "a.md"?\n\nRecoverable.');
  });
});

describe("no raw window.confirm calls", () => {
  it("app code goes through confirmDialog", () => {
    const srcDir = join(__dirname, "..", "..", "src");
    const files: string[] = [];
    const walk = (dir: string) => {
      for (const name of readdirSync(dir)) {
        const path = join(dir, name);
        if (statSync(path).isDirectory()) walk(path);
        else if (/\.(ts|tsx)$/.test(name)) files.push(path);
      }
    };
    walk(srcDir);
    const offenders: string[] = [];
    for (const file of files) {
      const rel = relative(srcDir, file).split(sep).join("/");
      if (rel === "lib/confirm.ts") continue;
      readFileSync(file, "utf8")
        .split("\n")
        .forEach((line, i) => {
          const code = line.replace(/\/\/.*$/, "");
          if (/\bwindow\.confirm\s*\(/.test(code)) offenders.push(`src/${rel}:${i + 1}`);
        });
    }
    expect(offenders, "use confirmDialog() from src/lib/confirm.ts — the Tauri dialog plugin makes window.confirm async").toEqual([]);
  });
});
