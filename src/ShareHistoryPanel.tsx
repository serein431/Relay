import { useEffect, useState } from "react";
import { copyText } from "./lib/clipboard";
import { userErrorMessage } from "./lib/errors";
import {
  listShareHistory,
  resumeSavedShareUpload,
  revokeSavedShare,
} from "./lib/tauri";
import LoadingState from "./LoadingState";
import type { ShareHistoryRecord } from "./types";

type ShareHistoryPanelProps = {
  refreshKey: number;
  onNotice: (message: string) => void;
};

function formatDate(value: string): string {
  const time = new Date(value);
  return Number.isNaN(time.getTime()) ? value : time.toLocaleString("zh-CN");
}

function isExpired(record: ShareHistoryRecord): boolean {
  const time = new Date(record.expires_at).getTime();
  return !Number.isNaN(time) && time <= Date.now();
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

function hasEnded(record: ShareHistoryRecord): boolean {
  return record.status === "revoked" || isExpired(record);
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
  const [manualCopy, setManualCopy] = useState<string | null>(null);
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
        if (active) setError(userErrorMessage(caught, "无法读取分享记录，请稍后重试。"));
      })
      .finally(() => {
        if (active) setLoading(false);
      });
    return () => {
      active = false;
    };
  }, [refreshKey, reloadKey]);

  const copy = async (record: ShareHistoryRecord) => {
    setManualCopy(null);
    try {
      await copyText(record.share_url);
      onNotice("分享链接已复制。");
    } catch {
      setManualCopy(record.share_id);
      onNotice("自动复制失败，请在当前记录中手动复制链接。");
    }
  };

  const revoke = async (record: ShareHistoryRecord) => {
    if (confirming !== record.share_id) {
      setConfirming(record.share_id);
      return;
    }
    setRevoking(record.share_id);
    setConfirming(null);
    setManualCopy(null);
    setError(null);
    try {
      const result = await revokeSavedShare({ share_id: record.share_id });
      setRecords((current) =>
        current.map((item) =>
          item.share_id === result.record.share_id ? result.record : item,
        ),
      );
      onNotice("分享已撤销，旧链接不能再打开分享内容。");
    } catch (caught) {
      setError(userErrorMessage(caught, "无法撤销分享链接，请稍后重试。"));
    } finally {
      setRevoking(null);
    }
  };

  const resume = async (record: ShareHistoryRecord) => {
    setResuming(record.share_id);
    setConfirming(null);
    setManualCopy(null);
    setError(null);
    try {
      const result = await resumeSavedShareUpload({ share_id: record.share_id });
      setRecords((current) =>
        current.map((item) =>
          item.share_id === result.record.share_id ? result.record : item,
        ),
      );
      try {
        await copyText(result.record.share_url);
        onNotice("文件已上传，分享链接已复制。");
      } catch {
        setManualCopy(result.record.share_id);
        onNotice("文件已上传。自动复制失败，请在当前记录中手动复制链接。");
      }
    } catch (caught) {
      setError(userErrorMessage(caught, "无法继续上传，请稍后重试。"));
    } finally {
      setResuming(null);
    }
  };

  const currentRecords = records.filter((record) => !hasEnded(record));
  const endedRecords = records.filter(hasEnded);

  const renderRecord = (record: ShareHistoryRecord) => {
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
            <h2 title={record.project_title ?? "未命名会话"}>
              {record.project_title ?? "未命名会话"}
            </h2>
          </div>
          <span className={`history-status ${status.className}`}>{status.label}</span>
        </header>

        {manualCopy === record.share_id && status.available ? (
          <label className="history-manual-copy">
            <span>手动复制链接</span>
            <textarea
              readOnly
              value={record.share_url}
              onFocus={(event) => event.currentTarget.select()}
            />
          </label>
        ) : null}

        <dl className="history-metadata">
          <div><dt>创建</dt><dd>{formatDate(record.created_at)}</dd></div>
          <div><dt>到期</dt><dd>{formatDate(record.expires_at)}</dd></div>
        </dl>

        <footer>
          {record.status === "pending_upload" ? (
            <div className="history-card-actions">
              <button
                type="button"
                className="is-resume"
                onClick={() => void resume(record)}
                disabled={!record.package_exists || resuming !== null || isRevoking}
                title={record.package_exists ? undefined : "本地分享文件已移动或删除，只能撤销"}
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
                {isRevoking ? "正在撤销" : isConfirming ? "确认撤销" : "撤销"}
              </button>
            </div>
          ) : status.available ? (
            <div className="history-card-actions">
              <button
                type="button"
                className="is-copy"
                onClick={() => void copy(record)}
              >
                复制链接
              </button>
              <button
                type="button"
                className={isConfirming ? "is-confirming" : ""}
                onClick={() => void revoke(record)}
                onBlur={() => setConfirming((value) => value === record.share_id ? null : value)}
                disabled={isRevoking || resuming !== null}
              >
                {isRevoking ? "正在撤销" : isConfirming ? "确认撤销" : "撤销链接"}
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
  };

  return (
    <section className="share-history-shell">
      <main className="share-history-main">
        <header className="history-heading">
          <h1>分享记录</h1>
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
            <strong>无法读取分享记录</strong>
            <p>{error}</p>
            <small>已有分享不会因此失效。</small>
          </div>
        ) : null}

        {loading && records.length === 0 ? (
          <LoadingState
            className="history-loading-state"
            title="正在读取分享记录"
            description="Relay 正在检查本机保存的分享链接及其当前状态。"
            stages={["读取本机记录", "核对上传状态", "整理可用链接"]}
          />
        ) : null}

        {!loading && !error && records.length === 0 ? (
          <div className="history-empty">
            <h2>还没有创建分享链接</h2>
            <p>从会话页面选择一条会话并创建分享链接。</p>
          </div>
        ) : null}

        {currentRecords.length > 0 ? (
          <div className="history-list">{currentRecords.map(renderRecord)}</div>
        ) : null}

        {endedRecords.length > 0 ? (
          <details className="history-ended-group">
            <summary>
              <span>已结束</span>
              <small>{endedRecords.length} 条</small>
            </summary>
            <div className="history-list">{endedRecords.map(renderRecord)}</div>
          </details>
        ) : null}
      </main>
    </section>
  );
}
