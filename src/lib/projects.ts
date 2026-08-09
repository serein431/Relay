import type { ProjectGroup, SessionSummary } from "../types";
import { sessionDateTitle } from "./sessions";

const UNKNOWN_PROJECT_NAMES = new Set(["", "unknown project", "未归类项目", "未命名项目"]);

function projectDisplayName(session: SessionSummary): string {
  const name = session.projectName.trim();
  if (!UNKNOWN_PROJECT_NAMES.has(name.toLocaleLowerCase("zh-CN"))) return name;
  return sessionDateTitle(session.createdAt ?? session.updatedAt) || "临时项目";
}

export function groupProjects(sessions: SessionSummary[]): ProjectGroup[] {
  const groups = new Map<string, ProjectGroup>();
  for (const session of sessions) {
    const projectKey = session.projectKey?.trim();
    const key = projectKey
      ? `project:${projectKey}`
      : `legacy:${JSON.stringify([session.projectName, session.cwd])}`;
    const current = groups.get(key) ?? {
      id: key,
      name: projectDisplayName(session),
      path: session.projectRoot ?? session.cwd,
      sessions: [],
    };
    current.sessions.push(session);
    groups.set(key, current);
  }
  return [...groups.values()].sort((a, b) => a.name.localeCompare(b.name, "zh-CN"));
}
