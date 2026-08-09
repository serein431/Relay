import type { EnvironmentStatus, SessionSummary } from "./types";

export const demoEnvironment: EnvironmentStatus = {
  git: { installed: true, version: "2.50.1" },
  claudeCode: { installed: true, version: "2.1.205" },
  codex: { installed: true, version: "0.142.5" },
  adapter: { installed: false },
  home: "/Users/demo",
};

export const demoSessions: SessionSummary[] = [
  {
    id: "claude-relay-01",
    agent: "claude_code",
    title: "恢复分享包中的 Git 变更",
    projectKey: "repo:relay-demo",
    projectName: "Relay",
    projectRoot: "/Users/demo/Projects/relay",
    workspace: "main",
    cwd: "/Users/demo/Projects/relay",
    updatedAt: "2026-08-07T09:42:00+08:00",
    preview:
      "已确认新 Git 工作树的恢复顺序，下一步处理已暂存与未暂存变更的冲突。",
    messageCount: 36,
    health: "complete",
    warnings: [],
  },
  {
    id: "codex-relay-02",
    agent: "codex",
    title: "检查 Claude 与 ChatGPT 的会话格式",
    projectKey: "repo:relay-demo",
    projectName: "Relay",
    projectRoot: "/Users/demo/Projects/relay",
    workspace: "adapter-spike",
    cwd: "/Users/demo/Projects/relay-adapter",
    updatedAt: "2026-08-07T08:18:00+08:00",
    preview:
      "已保留 ChatGPT 工具调用结构；发现两个无法配对的调用结果，导出完整性为“部分”。",
    messageCount: 54,
    health: "partial",
    warnings: ["发现 2 个无法配对的工具结果"],
  },
  {
    id: "claude-notes-03",
    agent: "claude_code",
    title: "整理离线笔记的全文检索",
    projectKey: "repo:context-notes-demo",
    projectName: "Context Notes",
    projectRoot: "/Users/demo/Projects/context-notes",
    workspace: "main",
    cwd: "/Users/demo/Projects/context-notes",
    updatedAt: "2026-08-06T22:07:00+08:00",
    preview:
      "已经完成标题与正文索引，下一步检查中文分词和增量更新是否正确。",
    messageCount: 21,
    health: "complete",
    warnings: [],
  },
];
