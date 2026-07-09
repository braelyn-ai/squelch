// A sitrep band: header (glyph + title + count + optional subtitle), its rows,
// and a band-specific empty state. STANDING/NEW/STILL OPEN each render through
// this; the STILL OPEN band passes aging so rows escalate with age.

import { TriangleAlert, Sparkles, Hourglass, type LucideIcon } from "lucide-react";
import type { AttentionUpdate } from "../api";
import { UpdateRow } from "./UpdateRow";
import { openWeight } from "../lib/format";

export type BandVariant = "standing" | "new" | "open";

const META: Record<
  BandVariant,
  { title: string; Icon: LucideIcon; sub?: string; empty: string }
> = {
  standing: {
    title: "Standing",
    Icon: TriangleAlert,
    sub: "deadlines · immune to time",
    empty: "nothing outstanding — no deadlines or past-due items.",
  },
  new: {
    title: "Since last check",
    Icon: Sparkles,
    empty: "nothing new since anyone last looked.",
  },
  open: {
    title: "Still open",
    Icon: Hourglass,
    sub: "aging · escalating",
    empty: "clean — nothing left rotting.",
  },
};

export interface BandSectionProps {
  variant: BandVariant;
  items: AttentionUpdate[];
  selectedId: number | null;
  onSelect: (id: number) => void;
  onOpen: (u: AttentionUpdate) => void;
}

export function BandSection({
  variant,
  items,
  selectedId,
  onSelect,
  onOpen,
}: BandSectionProps) {
  const meta = META[variant];
  const aging = variant === "open";
  const Icon = meta.Icon;

  return (
    <section className={`band band-${variant}`}>
      <div className="band-head">
        <span className="glyph">
          <Icon size={14} />
        </span>
        <span>{meta.title}</span>
        <span className="count">({items.length})</span>
        {meta.sub && <span className="sub">— {meta.sub}</span>}
      </div>

      {items.length === 0 ? (
        <div className="band-empty">{meta.empty}</div>
      ) : (
        items.map((u) => (
          <UpdateRow
            key={u.id}
            update={u}
            selected={u.id === selectedId}
            aging={aging}
            weight={aging ? openWeight(u.surfaced_at, u.importance) : 0}
            onSelect={onSelect}
            onOpen={onOpen}
          />
        ))
      )}
    </section>
  );
}
