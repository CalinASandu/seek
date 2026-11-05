import { useEffect, useState } from "react";
import { useApi } from "../../api/context";
import type { HistoryEntry, PutBackResult } from "../../api/types";
import { fileName, folderOf, WHY_VERB } from "../../lib/clean";
import { formatRelative, formatSize } from "../../lib/format";
import { errText } from "../../lib/pcs";
import { Dialog } from "../Dialog";
import { MidPath, SkeletonRows } from "./parts";
import { VirtualRows } from "./VirtualRows";

/** What Seek has moved before, newest first. Put back is here; nothing here removes anything. */
export function RemovedSheet({ onClose, notify }: { onClose: () => void; notify: (m: string) => void }) {
  const api = useApi();
  const [entries, setEntries] = useState<HistoryEntry[] | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  const [back, setBack] = useState<Record<string, PutBackResult | "busy">>({});

  useEffect(() => {
    let live = true;
    api.cleanHistory().then(
      (h) => live && setEntries([...h].sort((a, b) => b.timeMs - a.timeMs)),
      (e: unknown) => live && setProblem(errText(e)),
    );
    return () => {
      live = false;
    };
  }, [api]);

  const putBack = async (id: string) => {
    setBack((cur) => ({ ...cur, [id]: "busy" }));
    try {
      const [r] = await api.cleanPutBack([id]);
      setBack((cur) => ({ ...cur, [id]: r ?? { id, restored: false, reason: "No answer." } }));
    } catch (e) {
      setBack((cur) => ({ ...cur, [id]: { id, restored: false, reason: errText(e) } }));
    }
  };

  return (
    <Dialog
      title="Recently removed"
      onClose={onClose}
      width={640}
      footer={
        <>
          <button className="btn" onClick={() => void api.openRecycleBin().catch(() => notify("Couldn't open the Recycle Bin"))}>
            Open Recycle Bin
          </button>
          <button className="btn btn-primary" data-autofocus onClick={onClose}>
            Done
          </button>
        </>
      }
    >
      {problem ? (
        <p className="review-warn">{problem}</p>
      ) : entries === null ? (
        <SkeletonRows count={5} />
      ) : entries.length === 0 ? (
        <p className="removed-empty">Nothing has been removed yet. What Seek moves to the Recycle Bin will be listed here.</p>
      ) : (
        <VirtualRows
          className="removed-list"
          role="list"
          rowRole="listitem"
          aria-label="Removed items"
          rows={entries}
          keyOf={(e) => e.id}
          size={() => 56}
          render={(e) => {
            const r = back[e.id];
            const done = r && r !== "busy" ? r : null;
            return (
              <div className="removed-row" title={e.originalPath}>
                <div className="removed-main">
                  <span className="removed-name">{fileName(e.originalPath)}</span>
                  <MidPath path={folderOf(e.originalPath)} className="removed-folder" />
                </div>
                <div className="removed-meta">
                  <span className="tnum">{formatSize(e.size)}</span>
                  <span className="removed-when">
                    {WHY_VERB[e.why]} · {formatRelative(e.timeMs)}
                  </span>
                </div>
                <div className="removed-act">
                  {e.leftReason !== null ? (
                    <span className="removed-left" title={e.leftReason}>
                      {e.leftReason}
                    </span>
                  ) : done?.restored ? (
                    <span className="removed-done">Put back</span>
                  ) : done ? (
                    <span className="removed-left" title={done.reason ?? undefined}>
                      {done.reason}
                    </span>
                  ) : e.why === "uninstall" ? (
                    <span className="removed-left">An uninstaller's work can't be put back</span>
                  ) : (
                    <button className="btn btn-small" aria-label={`Put back ${fileName(e.originalPath)}`} disabled={r === "busy"} onClick={() => void putBack(e.id)}>
                      Put back
                    </button>
                  )}
                </div>
              </div>
            );
          }}
        />
      )}
    </Dialog>
  );
}
