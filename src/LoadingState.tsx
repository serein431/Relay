import type { CSSProperties } from "react";

type LoadingStateProps = {
  title: string;
  description: string;
  stages: string[];
  compact?: boolean;
  className?: string;
};

export default function LoadingState({
  title,
  description,
  stages,
  compact = false,
  className = "",
}: LoadingStateProps) {
  return (
    <section
      className={`operation-loading${compact ? " is-compact" : ""}${className ? ` ${className}` : ""}`}
      aria-busy="true"
      aria-live="polite"
    >
      <header>
        <span className="operation-loading-mark" aria-hidden="true"><i /><i /><i /></span>
        <div>
          <strong>{title}</strong>
          <p>{description}</p>
        </div>
      </header>

      <div
        className="operation-loading-progress"
        role="progressbar"
        aria-label={title}
        aria-valuetext={description}
      >
        <i />
      </div>

      <div className="operation-loading-stages" aria-label="当前处理内容">
        {stages.map((stage, index) => (
          <span key={stage} style={{ "--stage-index": index } as CSSProperties}>
            <i aria-hidden="true" />
            {stage}
          </span>
        ))}
      </div>

      <div className="operation-loading-skeleton" aria-hidden="true">
        <i />
        <span><b /><em /></span>
      </div>
    </section>
  );
}
