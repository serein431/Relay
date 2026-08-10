import { useEffect, useRef, useState } from "react";
import { copyText } from "./lib/clipboard";
import { userErrorMessage } from "./lib/errors";
import { revokeSavedShare, uploadShare } from "./lib/tauri";
import { DEFAULT_SHARE_SERVICE_BASE_URL } from "./lib/share-service";
import type { ExportRelaypackResult, UploadShareResult } from "./types";

type CloudShareActionsProps = {
  pack: ExportRelaypackResult;
  onNotice: (message: string) => void;
  onHistoryChanged?: () => void;
  autoUpload?: boolean;
  initialExpiresInSeconds?: number;
};

function isPendingUploadError(error: unknown): boolean {
  if (typeof error !== "object" || error === null) return false;
  const value = error as { details?: unknown };
  if (typeof value.details !== "object" || value.details === null) return false;
  const details = value.details as { upload_pending?: unknown; can_resume?: unknown };
  return details.upload_pending === true && details.can_resume === true;
}

export default function CloudShareActions({
  pack,
  onNotice,
  onHistoryChanged,
  autoUpload = false,
  initialExpiresInSeconds = 7 * 24 * 60 * 60,
}: CloudShareActionsProps) {
  const [expiresInSeconds, setExpiresInSeconds] = useState(initialExpiresInSeconds);
  const [share, setShare] = useState<UploadShareResult | null>(null);
  const [revoked, setRevoked] = useState(false);
  const [revokeConfirming, setRevokeConfirming] = useState(false);
  const [busy, setBusy] = useState<"upload" | "revoke" | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [manualCopy, setManualCopy] = useState(false);
  const autoUploadStarted = useRef(false);

  const upload = async () => {
    setBusy("upload");
    setError(null);
    try {
      const result = await uploadShare({
        package_path: pack.package_path,
        key: pack.key_fragment,
        service_base_url: DEFAULT_SHARE_SERVICE_BASE_URL,
        project_title: pack.preview.title,
        project_name: pack.preview.project_name,
        expires_in_seconds: expiresInSeconds,
      });
      setShare(result);
      setRevoked(false);
      setManualCopy(false);
      onHistoryChanged?.();
      try {
        await copyText(result.share_url);
        onNotice("分享链接已生成并复制。");
      } catch {
        setManualCopy(true);
        onNotice("分享链接已经生成。自动复制失败，请点击“复制链接”。");
      }
    } catch (caught) {
      if (isPendingUploadError(caught)) {
        onHistoryChanged?.();
        setError(
          "分享文件还没有上传完成。请到“分享记录”继续上传或撤销。",
        );
        onNotice("上传尚未完成，可以从分享记录继续或撤销。");
      } else {
        setError(userErrorMessage(caught, "无法生成分享链接，请稍后重试。"));
      }
    } finally {
      setBusy(null);
    }
  };

  useEffect(() => {
    if (!autoUpload || autoUploadStarted.current) return;
    autoUploadStarted.current = true;
    void upload();
  }, [autoUpload]);

  const copy = async () => {
    if (!share) return;
    try {
      await copyText(share.share_url);
      setManualCopy(false);
      onNotice("分享链接已复制。");
    } catch {
      setManualCopy(true);
      onNotice("自动复制失败，请在页面中手动复制链接。");
    }
  };

  const revoke = async () => {
    if (!share) return;
    if (!revokeConfirming) {
      setRevokeConfirming(true);
      window.setTimeout(() => setRevokeConfirming(false), 5000);
      return;
    }
    setBusy("revoke");
    setError(null);
    try {
      await revokeSavedShare({ share_id: share.share_id });
      setRevoked(true);
      setManualCopy(false);
      setRevokeConfirming(false);
      onHistoryChanged?.();
      onNotice("分享已撤销，旧链接不能再打开分享内容。");
    } catch (caught) {
      setError(userErrorMessage(caught, "无法撤销分享链接，请稍后重试。"));
    } finally {
      setBusy(null);
    }
  };

  return (
    <section className="cloud-share-actions" aria-label="分享链接">
      {share ? (
        <>
          <div className={`share-ready${revoked ? " is-revoked" : ""}`}>
            <strong>{revoked ? "分享链接已撤销" : "分享链接已生成"}</strong>
            <span>{revoked ? "接收者不能再打开这个链接。" : "链接已经复制，可以直接发送给接收者。"}</span>
            {!revoked ? (
              <small>有效至 {new Date(share.expires_at).toLocaleString("zh-CN")}</small>
            ) : null}
          </div>
          {manualCopy && !revoked ? (
            <label className="share-manual-copy">
              <span>手动复制链接</span>
              <textarea
                readOnly
                value={share.share_url}
                onFocus={(event) => event.currentTarget.select()}
              />
            </label>
          ) : null}
          <div className="cloud-share-buttons">
            <button type="button" onClick={() => void revoke()} disabled={busy !== null || revoked}>
              {busy === "revoke" ? "正在撤销" : revoked ? "已经撤销" : revokeConfirming ? "再点一次确认撤销" : "撤销链接"}
            </button>
            <button type="button" className="is-primary" onClick={() => void copy()} disabled={revoked}>
              复制链接
            </button>
          </div>
          <p className="cloud-share-note">链接包含查看权限，请只发送给需要查看的人。</p>
        </>
      ) : (
        <>
          {autoUpload ? (
            <p className="cloud-share-progress">
              {busy === "upload" ? "正在生成分享链接…" : "正在准备分享链接…"}
            </p>
          ) : (
            <>
              <p className="cloud-share-intro">链接有效期内，任何持有者都可以查看分享内容。</p>
              <div className="cloud-share-grid is-single">
                <label>
                  <span>有效期</span>
                  <select value={expiresInSeconds} onChange={(event) => setExpiresInSeconds(Number(event.target.value))}>
                    <option value={24 * 60 * 60}>1 天</option>
                    <option value={7 * 24 * 60 * 60}>7 天</option>
                    <option value={30 * 24 * 60 * 60}>30 天</option>
                  </select>
                </label>
              </div>
            </>
          )}
          {error ? <p className="cloud-share-error">{error}</p> : null}
          {!autoUpload || error ? (
            <button className="cloud-upload-button" type="button" onClick={() => void upload()} disabled={busy !== null}>
              {busy === "upload" ? "正在生成分享链接" : error ? "重新生成分享链接" : "生成分享链接"}
            </button>
          ) : null}
          <p className="cloud-share-note">内容加密后上传，分享服务不能读取聊天记录或代码。</p>
        </>
      )}
      {share && error ? <p className="cloud-share-error">{error}</p> : null}
    </section>
  );
}
