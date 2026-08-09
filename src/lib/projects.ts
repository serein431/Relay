import type { ProjectGroup, SessionSummary } from "../types";
import { sessionDateTitle } from "./sessions";

const UNKNOWN_PROJECT_NAMES = new Set(["", "unknown project", "未归类项目", "未命名项目"]);

function projectDisplayName(session: SessionSummary): string {
  const name = session.projectName.trim();
  if (!UNKNOWN_PROJECT_NAMES.has(name.toLocaleLowerCase("zh-CN"))) return name;
  return sessionDateTitle(session.createdAt ?? session.updatedAt) || "临时项目";
}

function sessionTime(session: SessionSummary): number {
  for (const value of [session.updatedAt, session.createdAt]) {
    if (!value) continue;
    const time = new Date(value).getTime();
    if (!Number.isNaN(time)) return time;
  }
  return Number.NEGATIVE_INFINITY;
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
  const projects = [...groups.values()];
  for (const project of projects) {
    project.sessions.sort((a, b) => sessionTime(b) - sessionTime(a));
    if (project.sessions[0]) project.name = projectDisplayName(project.sessions[0]);
  }
  return projects.sort((a, b) => {
    const aTime = sessionTime(a.sessions[0]!);
    const bTime = sessionTime(b.sessions[0]!);
    if (aTime !== bTime) return bTime > aTime ? 1 : -1;
    const nameDifference = a.name.localeCompare(b.name, "zh-CN");
    return nameDifference !== 0 ? nameDifference : a.id.localeCompare(b.id);
  });
}
