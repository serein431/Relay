import { describe, expect, it } from "vitest";
import { loadWorkspaceSnapshot, type WorkspaceRuntime } from "./tauri";

function runtime(
  native: boolean,
  handlers: Record<string, () => unknown | Promise<unknown>> = {},
): WorkspaceRuntime {
  return {
    isTauri: () => native,
    invoke: async (command) => {
      const handler = handlers[command];
      if (!handler) throw new Error(`unexpected command: ${command}`);
      return handler();
    },
  };
}

const environment = {
  tools: {
    git: { available: true, path: "/usr/bin/git" },
    claude: { available: true, path: "/usr/local/bin/claude" },
    codex: { available: true, path: "/usr/local/bin/codex" },
  },
  homes: {
    claude: { path: "/Users/test/.claude", exists: true },
    codex: { path: "/Users/test/.codex", exists: true },
  },
  adapter: { available: true, path: "/Applications/Relay.app/adapter" },
};

const sessions = {
  sessions: [{
    id: "session-1",
    provider: "codex",
    title: "继续开发 Relay",
    project_name: "Relay",
    cwd: "/Users/test/Relay",
  }],
  warnings: [],
};

describe("loadWorkspaceSnapshot", () => {
  it("只在普通浏览器中返回演示数据", async () => {
    const result = await loadWorkspaceSnapshot(runtime(false));
    expect(result.source).toBe("demo");
    expect(result.sessions.length).toBeGreaterThan(0);
    expect(result.issues).toEqual([]);
  });

  it("正式应用的单项失败不会丢掉其他真实结果", async () => {
    const result = await loadWorkspaceSnapshot(runtime(true, {
      environment_status: () => environment,
      adapter_health: () => Promise.reject({ code: "adapter_timeout", message: "读取组件响应超时" }),
      discover_sessions: () => sessions,
    }));

    expect(result.source).toBe("native");
    expect(result.sessions).toHaveLength(1);
    expect(result.environment.codex.installed).toBe(true);
    expect(result.issues).toEqual([expect.objectContaining({
      stage: "adapter_health",
      code: "adapter_timeout",
      message: "读取组件响应超时",
      severity: "error",
    })]);
  });

  it("正式应用扫描失败时返回空列表，不混入演示会话", async () => {
    const result = await loadWorkspaceSnapshot(runtime(true, {
      environment_status: () => environment,
      adapter_health: () => ({ executable_path: "/Applications/Relay.app/adapter" }),
      discover_sessions: () => Promise.reject({
        code: "adapter_not_found",
        message: "找不到随包的会话读取组件",
      }),
    }));

    expect(result.source).toBe("native");
    expect(result.sessions).toEqual([]);
    expect(result.issues).toContainEqual(expect.objectContaining({
      stage: "discover_sessions",
      code: "adapter_not_found",
      message: "找不到随包的会话读取组件",
    }));
  });

  it("保留会话扫描返回的目录和部分读取提示", async () => {
    const result = await loadWorkspaceSnapshot(runtime(true, {
      environment_status: () => environment,
      adapter_health: () => ({ executable_path: "/Applications/Relay.app/adapter" }),
      discover_sessions: () => ({
        sessions: [],
        warnings: [{ code: "claude_home_missing", message: "Claude projects directory does not exist" }],
      }),
    }));

    expect(result.source).toBe("native");
    expect(result.issues).toContainEqual(expect.objectContaining({
      code: "claude_home_missing",
      severity: "warning",
    }));
  });

  it("把超大会话说明为单条跳过，不显示英文内部错误", async () => {
    const result = await loadWorkspaceSnapshot(runtime(true, {
      environment_status: () => environment,
      adapter_health: () => ({ executable_path: "/Applications/Relay.app/adapter" }),
      discover_sessions: () => ({
        sessions: sessions.sessions,
        warnings: [{
          code: "session_too_large",
          message: "A session file exceeded the configured safety limit",
        }],
      }),
    }));

    expect(result.sessions).toHaveLength(1);
    expect(result.issues).toContainEqual(expect.objectContaining({
      code: "session_too_large",
      severity: "warning",
      message: expect.stringContaining("超过 256 MB"),
    }));
    expect(result.issues[0].message).not.toContain("configured safety limit");
  });
});
