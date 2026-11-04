/**
 * Settings store — agent profile picker integration.
 *
 * Verifies the profile list loads, the active profile saves through
 * `save_settings`, and the local signal updates so UI re-renders pick
 * up the change.
 */

import { describe, it, expect, beforeEach } from "vitest";
import { mockInvoke } from "./setup";
import { settingsStore } from "../../src/stores/settings";

describe("settings store — agent profile", () => {
  beforeEach(async () => {
    await settingsStore.load();
  });

  it("loads the profile list from the backend", async () => {
    await settingsStore.loadAgentProfiles();
    const profiles = settingsStore.agentProfiles();
    expect(profiles.length).toBe(4);
    expect(profiles.map((p) => p.id)).toEqual([
      "default",
      "quicklookup",
      "research",
      "writer",
    ]);
  });

  it("setAgentProfile persists via save_settings and updates local state", async () => {
    let receivedSettings: any;
    mockInvoke.set("save_settings", (args: any) => {
      receivedSettings = args.settings;
      return undefined;
    });
    await settingsStore.setAgentProfile("research");
    expect(receivedSettings.agent_profile).toBe("research");
    expect(settingsStore.settings().agent_profile).toBe("research");
  });

  it("default agent_profile in fresh settings is 'default'", () => {
    expect(settingsStore.settings().agent_profile).toBe("default");
  });
});
