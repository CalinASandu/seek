import { useEffect, useRef, type HTMLAttributes, type KeyboardEvent, type ReactNode } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";

interface Props<R> extends Omit<HTMLAttributes<HTMLDivElement>, "children" | "onKeyDown"> {
  rows: R[];
  /** Every row's height is known from what it is, so nothing is measured. */
  size: (row: R, index: number) => number;
  keyOf: (row: R) => string;
  render: (row: R, index: number) => ReactNode;
  /** The row that has keyboard focus: kept in view. */
  focusIndex?: number;
  rowRole?: string;
  rowClass?: (row: R, index: number) => string;
  onKeyDown?: (e: KeyboardEvent<HTMLDivElement>) => void;
  autoFocus?: boolean;
  /** The scrolling element, for the caller that returns DOM focus to it. */
  listRef?: { current: HTMLDivElement | null };
}

/** A scrolling list that renders only the rows in view, the same way as the results table. */
export function VirtualRows<R>({
  rows,
  size,
  keyOf,
  render,
  focusIndex,
  rowRole = "row",
  rowClass,
  className = "",
  onKeyDown,
  autoFocus,
  listRef,
  ...rest
}: Props<R>) {
  const scroller = useRef<HTMLDivElement | null>(null);
  const virt = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scroller.current,
    estimateSize: (i) => size(rows[i]!, i),
    getItemKey: (i) => keyOf(rows[i]!),
    overscan: 10,
    initialRect: { width: 700, height: 420 },
  });

  useEffect(() => {
    if (focusIndex !== undefined && focusIndex >= 0 && focusIndex < rows.length) virt.scrollToIndex(focusIndex, { align: "auto" });
    // only when focus moves, not on every render of the rows
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [focusIndex]);

  return (
    <div
      ref={(el) => {
        scroller.current = el;
        if (listRef) listRef.current = el;
      }}
      className={`vrows ${className}`}
      tabIndex={0}
      data-autofocus={autoFocus ? "" : undefined}
      onKeyDown={onKeyDown}
      {...rest}
    >
      <div className="vrows-body" style={{ height: virt.getTotalSize() }}>
        {virt.getVirtualItems().map((v) => {
          const row = rows[v.index]!;
          return (
            <div
              key={v.key}
              role={rowRole}
              aria-rowindex={v.index + 1}
              className={`vrow ${rowClass?.(row, v.index) ?? ""}`}
              style={{ height: v.size, transform: `translateY(${v.start}px)` }}
            >
              {render(row, v.index)}
            </div>
          );
        })}
      </div>
    </div>
  );
}
