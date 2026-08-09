import { useEffect, useMemo, useState } from "react";
import {
  listShareHistory,
  resumeSavedShareUpload,
  revokeSavedShare,
} from "./lib/tauri";
import type { ShareHistoryRecord } from "./types";

type ShareHistoryPanelProps = {
  refreshKey: number;
  onNotice: (message: string) => void;
};

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  if (typeof error === "object" && error !== null && "message" in error) {
    return String((error as { message: unknown }).message);
  }
  return String(error);
}

function formatBytes(value: number): string {
  if (value < 1024) return `${value} B`;
  if (value < 1024 * 1024) return `${(value / 1024).toFixed(1)} KB`;
  return `${(value / (1024 * 1024)).toFixed(1)} MB`;
}

function formatDate(value: string): string {
  const time = new Date(value);
  return Number.isNaN(time.getTime()) ? value : time.toLocaleString("zh-CN");
}

function isExpired(record: ShareHistoryRecord): boolean {
  const time = new Date(record.expires_at).getTime();
  return !Number.isNaN(time) && time <= Date.now();
}

function displayLink(value: string): string {
  const fragment = value.indexOf("#k=");
  return fragment < 0 ? value : `${value.slice(0, fragment)}#k=••••••••`;
}

function statusOf(record: ShareHistoryRecord): {
  className: string;
  label: string;
  available: boolean;
} {
  if (record.status === "revoked") {
    return { className: "is-revoked", label: "已撤销", available: false };
  }
  if (record.status === "pending_upload") {
    return { className: "is-pending", label: "上传未完成", available: false };
  }
  if (isExpired(record)) {
    return { className: "is-expired", label: "已到期", available: false };
  }
  return { className: "is-active", label: "可使用", available: true };
}

export default function ShareHistoryPanel({
  refreshKey,
  onNotice,
}: ShareHistoryPanelProps) {
  const [records, setRecords] = useState<ShareHistoryRecord[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [revoking, setRevoking] = useState<string | null>(null);
  const [resuming, setResuming] = useState<string | null>(null);
  const [confirming, setConfirming] = useState<string | null>(null);
  const [reloadKey, setReloadKey] = useState(0);

  useEffect(() => {
    let active = true;
    setLoading(true);
    setError(null);
    void listShareHistory()
      .then((result) => {
        if (active) setRecords(result.records);
      })
      .catch((caught) => {
        if (active) setError(errorMessage(caught));
      })
      .finally(() => {
        if (active) setLoading(false);
      });
    return () => {
      active = false;
    };
  }, [refreshKey, reloadKey]);

  const counts = useMemo(() => {
    let active = 0;
    let pending = 0;
    let expired = 0;
    let revoked = 0;
    for (const record of records) {
      if (record.status === "revoked") revoked += 1;
      else if (record.status === "pending_upload") pending += 1;
      else if (isExpired(record)) expired += 1;
      else active += 1;
    }
    return { active, pending, expired, revoked };
  }, [records]);

  const copy = async (record: ShareHistoryRecord) => {
    await navigator.clipboard.writeText(record.share_url);
    onNotice("完整分享链接已复制。");
  };

  const revoke = async (record: ShareHistoryRecord) => {
    if (confirming !== record.share_id) {
      setConfirming(record.share_id);
      return;
    }
    setRevoking(record.share_id);
    setConfirming(null);
    setError(null);
    try {
      const result = await revokeSavedShare({ share_id: record.share_id });
      setRecords((current) =>
        current.map((item) =>
          item.share_id === result.record.share_id ? result.record : item,
        ),
      );
      onNotice("这个分享已经撤销，原链接不能再下载密文包。");
    } catch (caught) {
      setError(errorMessage(caught));
    } finally {
      setRevoking(null);
    }
  };

  const resume = async (record: ShareHistoryRecord) => {
    setResuming(record.share_id);
    setConfirming(null);
    setError(null);
    try {
      const result = await resumeSavedShareUpload({ share_id: record.share_id });
      setRecords((current) =>
        current.map((item) =>
          item.share_id === result.record.share_id ? result.record : item,
        ),
      );
      await navigator.clipboard.writeText(result.record.share_url);
      onNotice("密文已经上传，完整分享链接已复制。");
    } catch (caught) {
      setError(errorMessage(caught));
    } finally {
      setResuming(null);
    }
  };

  return (
    <section className="share-history-shell">
      <aside className="share-history-sidebar">
        <div className="pane-heading">
          <div>
            <span className="eyebrow">本机记录</span>
            <h2>分享记录</h2>
          </div>
          <span className="count-chip">{records.length}</span>
        </div>

        <div className="history-counts">
          <div><strong>{counts.active}</strong><span>可使用</span></div>
          <div><strong>{counts.pending}</strong><span>待上传</span></div>
          <div><strong>{counts.expired}</strong><span>已到期</span></div>
          <div><strong>{counts.revoked}</strong><span>已撤销</span></div>
        </div>

        <div className="history-security-note">
          <strong>撤销令牌留在本机</strong>
          <p>Relay 会先保存上传与撤销凭据，再传密文。中断后可以从这里继续或撤销，文件权限为 0600。</p>
          <p>这里不会写入 ~/.claude 或 ~/.codex。</p>
        </div>
      </aside>

      <main className="share-history-main">
        <header className="history-heading">
          <div>
            <span className="eyebrow">加密分享链接</span>
            <h1>已创建的分享链接</h1>
            <p>可以重新复制仍然有效的链接，也可以用本机保存的撤销令牌立即停用。</p>
          </div>
          <button
            type="button"
            className="history-refresh"
            onClick={() => setReloadKey((value) => value + 1)}
            disabled={loading}
          >
            {loading ? "正在读取" : "重新读取"}
          </button>
        </header>

        {error ? (
          <div className="history-error">
            <strong>分享记录暂时无法使用</strong>
            <p>{error}</p>
            <small>如果记录文件损坏，Relay 会停止写入，不会直接覆盖原文件。</small>
          </div>
        ) : null}

        {loading && records.length === 0 ? (
          <div className="history-empty"><i /><h2>正在读取本机记录</h2></div>
        ) : null}

        {!loading && !error && records.length === 0 ? (
          <div className="history-empty">
            <span>00</span>
            <h2>还没有分享记录</h2>
            <p>先从一条项目会话生成 .relaypack，再上传为加密链接。</p>
          </div>
        ) : null}

        <div className="history-list">
          {records.map((record) => {
            const status = statusOf(record);
            const isConfirming = confirming === record.share_id;
            const isRevoking = revoking === record.share_id;
            return (
              <article
                className={`history-card${record.status === "pending_upload" ? " is-pending" : ""}`}
                key={record.share_id}
              >
                <header>
                  <div>
                    <span
                      className="history-project"
                      title={record.project_name ?? "未命名项目"}
                    >
                      {record.project_name ?? "未命名项目"}
                    </span>
                    <h2 title={record.project_title ?? "未命名交接"}>
                      {record.project_title ?? "未命名交接"}
                    </h2>
                  </div>
                  <span className={`history-status ${status.className}`}>{status.label}</span>
                </header>

                <div className="history-link-row">
                  <code>{displayLink(record.share_url)}</code>
                  <button
                    type="button"
                    onClick={() => void copy(record)}
                    disabled={!status.available}
                  >
                    {record.status === "pending_upload" ? "尚未上传" : "复制链接"}
                  </button>
                </div>

                <dl className="history-metadata">
                  <div><dt>创建</dt><dd>{formatDate(record.created_at)}</dd></div>
                  <div><dt>到期</dt><dd>{formatDate(record.expires_at)}</dd></div>
                  <div><dt>密文</dt><dd>{formatBytes(record.ciphertext_bytes)}</dd></div>
                  <div><dt>服务</dt><dd>{record.service_base_url}</dd></div>
                </dl>

                <div className="history-package-row">
                  <span className={record.package_exists ? "is-present" : "is-missing"}>
                    {record.package_exists ? "本地包仍在" : "本地包已移动或删除"}
                  </span>
                  <code>{record.package_path}</code>
                </div>

                <footer>
                  <code>ID {record.share_id}</code>
                  {record.status === "pending_upload" ? (
                    <div className="history-card-actions">
                      <button
                        type="button"
                        className="is-resume"
                        onClick={() => void resume(record)}
                        disabled={!record.package_exists || resuming !== null || isRevoking}
                        title={record.package_exists ? undefined : "本地密文包已移动或删除，只能撤销"}
                      >
                        {resuming === record.share_id ? "正在继续" : "继续上传"}
                      </button>
                      <button
                        type="button"
                        className={isConfirming ? "is-confirming" : ""}
                        onClick={() => void revoke(record)}
                        onBlur={() => setConfirming((value) => value === record.share_id ? null : value)}
                        disabled={isRevoking || resuming !== null}
                      >
                        {isRevoking ? "正在撤销" : isConfirming ? "再次点击确认" : "撤销"}
                      </button>
                    </div>
                  ) : status.available ? (
                    <div className="history-card-actions">
                      <button
                        type="button"
                        className={isConfirming ? "is-confirming" : ""}
                        onClick={() => void revoke(record)}
                        onBlur={() => setConfirming((value) => value === record.share_id ? null : value)}
                        disabled={isRevoking || resuming !== null}
                      >
                        {isRevoking ? "正在撤销" : isConfirming ? "再次点击确认" : "撤销链接"}
                      </button>
                    </div>
                  ) : (
                    <span className="history-ended-at">
                      {record.revoked_at ? `撤销于 ${formatDate(record.revoked_at)}` : status.label}
                    </span>
                  )}
                </footer>
              </article>
            );
          })}
        </div>
      </main>
    </section>
  );
}
